//! Theme family library and editor.
//!
//! Each family owns a complete light/dark pair. Built-ins are immutable;
//! duplicating one creates an editable user file in `~/.ochub/themes/`.

use anyhow::{anyhow, Context as _, Result};
use gpui::{
    canvas, div, linear_color_stop, linear_gradient, point, prelude::*, px, size, Background,
    Bounds, ColorSpace, Context, Entity, FontWeight, PathBuilder, PathPromptOptions, Pixels, Point,
    Rgba, SharedString, Window,
};
use ochub_core::settings::{self, ThemeMode};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::icons::IconName;
use crate::layout;
use crate::text_input::TextInput;
use crate::theme::{self, Theme, ThemeColor, ThemeFamily, ThemeRecord, THEME_TOKENS};

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorVariant {
    Light,
    Dark,
}

struct ThemeEditor {
    family: ThemeFamily,
    variant: EditorVariant,
    name: Entity<TextInput>,
    author: Entity<TextInput>,
    description: Entity<TextInput>,
    light_colors: Vec<Entity<TextInput>>,
    dark_colors: Vec<Entity<TextInput>>,
}

pub struct ThemeView {
    registry: theme::ThemeRegistry,
    selected_family: String,
    mode: ThemeMode,
    status: Option<SharedString>,
    editor: Option<ThemeEditor>,
    confirm_delete: Option<String>,
}

impl ThemeView {
    pub(crate) fn shortcut_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_delete.is_some() {
            window.play_system_bell();
        } else if self.editor.is_some() {
            self.save_editor(window, cx);
        } else {
            window.play_system_bell();
        }
    }

    pub(crate) fn shortcut_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_delete.take().is_some() {
            cx.notify();
        } else if self.editor.is_some() {
            self.cancel_editor(window, cx);
        } else {
            window.play_system_bell();
        }
    }

    pub fn new(_cx: &mut Context<Self>) -> Self {
        let settings = settings::get_settings();
        let registry = theme::load_registry();
        let selected_family = if registry
            .themes
            .iter()
            .any(|record| record.family.id == settings.theme_family)
        {
            settings.theme_family
        } else {
            theme::DEFAULT_THEME_FAMILY.to_string()
        };
        let status = (!registry.diagnostics.is_empty()).then(|| {
            SharedString::from(format!(
                "有 {} 个用户主题未能加载，可检查主题文件格式。",
                registry.diagnostics.len()
            ))
        });
        Self {
            registry,
            selected_family,
            mode: settings.theme_mode,
            status,
            editor: None,
            confirm_delete: None,
        }
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.registry = theme::load_registry();
        let settings = settings::get_settings();
        self.selected_family = settings.theme_family;
        self.mode = settings.theme_mode;
        if !self
            .registry
            .themes
            .iter()
            .any(|record| record.family.id == self.selected_family)
        {
            self.selected_family = theme::DEFAULT_THEME_FAMILY.to_string();
        }
        self.status = (!self.registry.diagnostics.is_empty()).then(|| {
            SharedString::from(format!(
                "有 {} 个用户主题未能加载。",
                self.registry.diagnostics.len()
            ))
        });
        cx.notify();
    }

    fn set_status(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = Some(message.into());
        cx.notify();
    }

    fn persist_selection(&self) -> Result<()> {
        let family = self.selected_family.clone();
        let mode = self.mode;
        settings::mutate_settings(move |settings| {
            settings.theme_family = family;
            settings.theme_mode = mode;
        })?;
        Ok(())
    }

    fn apply_selection(
        &mut self,
        family_id: String,
        mode: ThemeMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(family) = self
            .registry
            .themes
            .iter()
            .find(|record| record.family.id == family_id)
            .map(|record| record.family.clone())
        else {
            self.set_status("主题不存在或已被移除", cx);
            return;
        };

        self.selected_family = family_id;
        self.mode = mode;
        if let Err(err) = self.persist_selection() {
            self.set_status(format!("保存主题选择失败: {err}"), cx);
            return;
        }
        theme::install_family(&family, mode, window.appearance());
        cx.refresh_windows();
        self.set_status(format!("已应用 {}", family.name), cx);
    }

    fn set_mode(&mut self, mode: ThemeMode, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_selection(self.selected_family.clone(), mode, window, cx);
    }

    fn restore_saved_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = settings::get_settings();
        theme::install_selected(
            &settings.theme_family,
            settings.theme_mode,
            window.appearance(),
        );
        cx.refresh_windows();
    }

    fn make_input(
        cx: &mut Context<Self>,
        placeholder: &'static str,
        value: String,
    ) -> Entity<TextInput> {
        cx.new(move |cx| {
            let mut input = TextInput::new(cx, placeholder);
            input.set_content(value, cx);
            input
        })
    }

    fn make_editor(&self, family: ThemeFamily, cx: &mut Context<Self>) -> ThemeEditor {
        let name = Self::make_input(cx, "主题名称", family.name.clone());
        let author = Self::make_input(cx, "作者", family.author.clone());
        let description = Self::make_input(cx, "主题说明", family.description.clone());
        let light_colors = THEME_TOKENS
            .iter()
            .map(|descriptor| {
                Self::make_input(cx, "#RRGGBB", family.light.color(descriptor.token).hex())
            })
            .collect();
        let dark_colors = THEME_TOKENS
            .iter()
            .map(|descriptor| {
                Self::make_input(cx, "#RRGGBB", family.dark.color(descriptor.token).hex())
            })
            .collect();
        ThemeEditor {
            family,
            variant: EditorVariant::Light,
            name,
            author,
            description,
            light_colors,
            dark_colors,
        }
    }

    fn open_editor(&mut self, family_id: &str, cx: &mut Context<Self>) {
        let Some(record) = self
            .registry
            .themes
            .iter()
            .find(|record| record.family.id == family_id)
            .cloned()
        else {
            self.set_status("找不到要编辑的主题", cx);
            return;
        };
        if record.built_in {
            self.set_status("内置主题只读，请先复制再编辑", cx);
            return;
        }
        self.editor = Some(self.make_editor(record.family, cx));
        self.status = None;
        cx.notify();
    }

    fn input_value(input: &Entity<TextInput>, cx: &mut Context<Self>) -> String {
        input.read(cx).content().trim().to_string()
    }

    fn sync_editor(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let editor = self
            .editor
            .as_mut()
            .ok_or_else(|| anyhow!("没有正在编辑的主题"))?;
        editor.family.name = Self::input_value(&editor.name, cx);
        editor.family.author = Self::input_value(&editor.author, cx);
        editor.family.description = Self::input_value(&editor.description, cx);

        for (index, descriptor) in THEME_TOKENS.iter().enumerate() {
            let light = ThemeColor::parse(&Self::input_value(&editor.light_colors[index], cx))
                .with_context(|| format!("浅色 · {}", descriptor.label))?;
            let dark = ThemeColor::parse(&Self::input_value(&editor.dark_colors[index], cx))
                .with_context(|| format!("深色 · {}", descriptor.label))?;
            editor.family.light.set_color(descriptor.token, light);
            editor.family.dark.set_color(descriptor.token, dark);
        }
        theme::validate_family(&editor.family)
    }

    fn preview_editor(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = self.sync_editor(cx) {
            self.set_status(format!("无法预览: {err}"), cx);
            return;
        }
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let (palette, dark) = match editor.variant {
            EditorVariant::Light => (editor.family.light, false),
            EditorVariant::Dark => (editor.family.dark, true),
        };
        theme::install(palette, dark);
        cx.refresh_windows();
        self.set_status("正在预览草稿；保存或取消后会退出预览", cx);
    }

    fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor = None;
        self.restore_saved_theme(window, cx);
        self.status = None;
        cx.notify();
    }

    fn save_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(err) = self.sync_editor(cx) {
            self.set_status(format!("保存失败: {err}"), cx);
            return;
        }
        let Some(family) = self.editor.as_ref().map(|editor| editor.family.clone()) else {
            return;
        };
        if let Err(err) = theme::save_user_family(&family) {
            self.set_status(format!("保存失败: {err}"), cx);
            return;
        }
        self.registry = theme::load_registry();
        self.editor = None;
        self.apply_selection(family.id.clone(), self.mode, window, cx);
        self.set_status(format!("已保存并应用 {}", family.name), cx);
    }

    fn duplicate_and_edit(&mut self, family_id: &str, cx: &mut Context<Self>) {
        let Some(source) = self
            .registry
            .themes
            .iter()
            .find(|record| record.family.id == family_id)
            .map(|record| record.family.clone())
        else {
            self.set_status("找不到要复制的主题", cx);
            return;
        };
        match theme::duplicate_family(&source) {
            Ok(family) => {
                self.editor = Some(self.make_editor(family, cx));
                self.status = None;
                cx.notify();
            }
            Err(err) => self.set_status(format!("复制主题失败: {err}"), cx),
        }
    }

    fn import_theme(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("导入 OCHub 主题".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let Some(path) = path else {
                return;
            };
            let result = cx
                .background_spawn(async move { theme::import_family(&path) })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(family) => {
                    this.registry = theme::load_registry();
                    this.status = Some(SharedString::from(format!(
                        "已导入 {}，浅色与深色配色均通过校验。",
                        family.name
                    )));
                    cx.notify();
                }
                Err(err) => this.set_status(format!("导入主题失败: {err}"), cx),
            })
            .ok();
        })
        .detach();
    }

    fn export_theme(&mut self, family_id: &str, cx: &mut Context<Self>) {
        let Some(family) = self
            .registry
            .themes
            .iter()
            .find(|record| record.family.id == family_id)
            .map(|record| record.family.clone())
        else {
            self.set_status("找不到要导出的主题", cx);
            return;
        };
        let directory = ochub_core::paths::get_app_config_dir();
        let suggested_name = format!("{}.ochub-theme.json", family.id);
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested_name));
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(path))) => Some(path),
                _ => None,
            };
            let Some(path) = path else {
                return;
            };
            let display_path = path.display().to_string();
            let result = cx
                .background_spawn(async move { theme::export_family(&family, &path) })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => this.set_status(format!("主题已导出到 {display_path}"), cx),
                Err(err) => this.set_status(format!("导出主题失败: {err}"), cx),
            })
            .ok();
        })
        .detach();
    }

    fn delete_confirmed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(family_id) = self.confirm_delete.take() else {
            return;
        };
        let Some(record) = self
            .registry
            .themes
            .iter()
            .find(|record| record.family.id == family_id)
            .cloned()
        else {
            self.set_status("主题已不存在", cx);
            return;
        };
        if let Err(err) = theme::delete_user_family(&record) {
            self.set_status(format!("删除主题失败: {err}"), cx);
            return;
        }
        if self.selected_family == family_id {
            self.selected_family = theme::DEFAULT_THEME_FAMILY.to_string();
            let _ = self.persist_selection();
            theme::install_selected(&self.selected_family, self.mode, window.appearance());
            cx.refresh_windows();
        }
        self.registry = theme::load_registry();
        self.set_status(format!("已删除 {}", record.family.name), cx);
    }

    fn mode_index(mode: ThemeMode) -> usize {
        match mode {
            ThemeMode::System => 0,
            ThemeMode::Light => 1,
            ThemeMode::Dark => 2,
        }
    }

    fn paint_preview_polygon(
        window: &mut Window,
        points: &[Point<Pixels>],
        background: Background,
    ) {
        let Some(first) = points.first().copied() else {
            return;
        };
        let mut builder = PathBuilder::fill();
        builder.move_to(first);
        for point in points.iter().skip(1) {
            builder.line_to(*point);
        }
        builder.line_to(first);
        if let Ok(path) = builder.build() {
            window.paint_path(path, background);
        }
    }

    fn preview_rounded_polygon(bounds: Bounds<Pixels>, radius: f32) -> Vec<Point<Pixels>> {
        let left = bounds.origin.x.as_f32();
        let top = bounds.origin.y.as_f32();
        let right = (bounds.origin.x + bounds.size.width).as_f32();
        let bottom = (bounds.origin.y + bounds.size.height).as_f32();
        let radius = radius
            .max(0.)
            .min(bounds.size.width.as_f32() / 2.)
            .min(bounds.size.height.as_f32() / 2.);

        if radius == 0. {
            return vec![
                point(px(left), px(top)),
                point(px(right), px(top)),
                point(px(right), px(bottom)),
                point(px(left), px(bottom)),
            ];
        }

        let half_pi = std::f32::consts::FRAC_PI_2;
        let corners = [
            (right - radius, top + radius, -half_pi, 0.),
            (right - radius, bottom - radius, 0., half_pi),
            (left + radius, bottom - radius, half_pi, half_pi * 2.),
            (left + radius, top + radius, half_pi * 2., half_pi * 3.),
        ];
        let mut points = Vec::with_capacity(20);
        for (center_x, center_y, start, end) in corners {
            for step in 0..=4 {
                let angle = start + (end - start) * step as f32 / 4.;
                points.push(point(
                    px(center_x + angle.cos() * radius),
                    px(center_y + angle.sin() * radius),
                ));
            }
        }
        points
    }

    fn preview_top_rounded_polygon(bounds: Bounds<Pixels>, radius: f32) -> Vec<Point<Pixels>> {
        let left = bounds.origin.x.as_f32();
        let top = bounds.origin.y.as_f32();
        let right = (bounds.origin.x + bounds.size.width).as_f32();
        let bottom = (bounds.origin.y + bounds.size.height).as_f32();
        let radius = radius
            .max(0.)
            .min(bounds.size.width.as_f32() / 2.)
            .min(bounds.size.height.as_f32());

        if radius == 0. {
            return vec![
                point(px(left), px(top)),
                point(px(right), px(top)),
                point(px(right), px(bottom)),
                point(px(left), px(bottom)),
            ];
        }

        let half_pi = std::f32::consts::FRAC_PI_2;
        let mut points = Vec::with_capacity(16);
        points.push(point(px(left + radius), px(top)));
        points.push(point(px(right - radius), px(top)));

        for step in 1..=4 {
            let angle = -half_pi + half_pi * step as f32 / 4.;
            points.push(point(
                px(right - radius + angle.cos() * radius),
                px(top + radius + angle.sin() * radius),
            ));
        }

        points.push(point(px(right), px(bottom)));
        points.push(point(px(left), px(bottom)));
        points.push(point(px(left), px(top + radius)));

        for step in 1..=4 {
            let angle = std::f32::consts::PI + half_pi * step as f32 / 4.;
            points.push(point(
                px(left + radius + angle.cos() * radius),
                px(top + radius + angle.sin() * radius),
            ));
        }

        points
    }

    fn preview_gradient(
        bounds: Bounds<Pixels>,
        split_top: Point<Pixels>,
        split_bottom: Point<Pixels>,
        light: Rgba,
        dark: Rgba,
    ) -> Background {
        let line_x = split_bottom.x.as_f32() - split_top.x.as_f32();
        let line_y = split_bottom.y.as_f32() - split_top.y.as_f32();
        let line_length = (line_x * line_x + line_y * line_y).sqrt().max(f32::EPSILON);

        // The right-hand normal points from the light region into the dark region.
        let normal_x = line_y / line_length;
        let normal_y = -line_x / line_length;
        let width = bounds.size.width.as_f32().max(1.);
        let height = bounds.size.height.as_f32().max(1.);

        // GPUI evaluates gradients relative to each path's own bounds. Compensate
        // for that aspect-ratio transform so every shape shares one global seam.
        let radians = (normal_y * width / height).atan2(normal_x);
        let angle = radians.to_degrees() + 90.;
        let mut direction_x = radians.cos();
        let mut direction_y = radians.sin();
        if width > height {
            direction_y *= height / width;
        } else {
            direction_x *= width / height;
        }
        let direction_length = (direction_x * direction_x + direction_y * direction_y).sqrt();
        direction_x /= direction_length;
        direction_y /= direction_length;

        let center_x = bounds.origin.x.as_f32() + width / 2.;
        let center_y = bounds.origin.y.as_f32() + height / 2.;
        let center_distance = (center_x - split_top.x.as_f32()) * direction_x
            + (center_y - split_top.y.as_f32()) * direction_y;
        let span = if direction_x.abs() > direction_y.abs() {
            width
        } else {
            height
        };
        let half_blend_width = 14.;
        let light_stop = (-center_distance - half_blend_width + span / 2.) / span;
        let dark_stop = (-center_distance + half_blend_width + span / 2.) / span;

        linear_gradient(
            angle,
            linear_color_stop(light, light_stop),
            linear_color_stop(dark, dark_stop),
        )
        .color_space(ColorSpace::Oklab)
    }

    fn paint_split_preview_shape(
        window: &mut Window,
        bounds: Bounds<Pixels>,
        radius: f32,
        split_top: Point<Pixels>,
        split_bottom: Point<Pixels>,
        light: Rgba,
        dark: Rgba,
    ) {
        let points = Self::preview_rounded_polygon(bounds, radius);
        let gradient = Self::preview_gradient(bounds, split_top, split_bottom, light, dark);
        Self::paint_preview_polygon(window, &points, gradient);
    }

    fn paint_top_rounded_split_preview_shape(
        window: &mut Window,
        bounds: Bounds<Pixels>,
        radius: f32,
        split_top: Point<Pixels>,
        split_bottom: Point<Pixels>,
        light: Rgba,
        dark: Rgba,
    ) {
        let points = Self::preview_top_rounded_polygon(bounds, radius);
        let gradient = Self::preview_gradient(bounds, split_top, split_bottom, light, dark);
        Self::paint_preview_polygon(window, &points, gradient);
    }

    fn preview_bounds(
        origin_x: Pixels,
        origin_y: Pixels,
        width: Pixels,
        height: Pixels,
    ) -> Bounds<Pixels> {
        Bounds {
            origin: point(origin_x, origin_y),
            size: size(width, height),
        }
    }

    fn inset_preview_bounds(bounds: Bounds<Pixels>, inset: Pixels) -> Bounds<Pixels> {
        Self::preview_bounds(
            bounds.origin.x + inset,
            bounds.origin.y + inset,
            bounds.size.width - inset * 2.,
            bounds.size.height - inset * 2.,
        )
    }

    fn paint_preview_card(
        window: &mut Window,
        bounds: Bounds<Pixels>,
        split_top: Point<Pixels>,
        split_bottom: Point<Pixels>,
        light: Theme,
        dark: Theme,
    ) {
        Self::paint_split_preview_shape(
            window,
            bounds,
            5.,
            split_top,
            split_bottom,
            light.border.rgba(),
            dark.border.rgba(),
        );
        Self::paint_split_preview_shape(
            window,
            Self::inset_preview_bounds(bounds, px(1.)),
            4.,
            split_top,
            split_bottom,
            light.surface.rgba(),
            dark.surface.rgba(),
        );
    }

    fn pair_preview(family: &ThemeFamily) -> impl IntoElement {
        let light = family.light;
        let dark = family.dark;
        canvas(
            move |_bounds, _window, _cx| (),
            move |bounds, _, window, _cx| {
                let left = bounds.origin.x;
                let top = bounds.origin.y;
                let right = bounds.origin.x + bounds.size.width;
                let bottom = bounds.origin.y + bounds.size.height;
                let split_top = point(left + bounds.size.width * 0.535, top);
                let split_bottom = point(left + bounds.size.width * 0.465, bottom);
                let header_height = px(18.);
                let sidebar_width = px(36.);
                let content_left = left + px(52.);
                let content_right = right - px(14.);
                let content_width = content_right - content_left;
                // The parent overflow mask is rectangular in GPUI, so the canvas must
                // cut its own top corners. Match rounded_lg (0.5rem) minus the 1px border.
                let preview_radius = (window.rem_size().as_f32() * 0.5 - 1.).max(0.);

                Self::paint_top_rounded_split_preview_shape(
                    window,
                    bounds,
                    preview_radius,
                    split_top,
                    split_bottom,
                    light.bg.rgba(),
                    dark.bg.rgba(),
                );
                Self::paint_top_rounded_split_preview_shape(
                    window,
                    Self::preview_bounds(left, top, bounds.size.width, header_height),
                    preview_radius,
                    split_top,
                    split_bottom,
                    light.header.rgba(),
                    dark.header.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(left, top + header_height, bounds.size.width, px(1.)),
                    0.,
                    split_top,
                    split_bottom,
                    light.border.rgba(),
                    dark.border.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(
                        left,
                        top + header_height + px(1.),
                        sidebar_width,
                        bottom - top - header_height - px(1.),
                    ),
                    0.,
                    split_top,
                    split_bottom,
                    light.mantle.rgba(),
                    dark.mantle.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(
                        left + sidebar_width,
                        top + header_height + px(1.),
                        px(1.),
                        bottom - top - header_height - px(1.),
                    ),
                    0.,
                    split_top,
                    split_bottom,
                    light.border.rgba(),
                    dark.border.rgba(),
                );

                // Header mark and navigation establish the app shell without
                // decorative browser chrome competing with the theme colors.
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(left + px(10.), top + px(5.), px(8.), px(8.)),
                    99.,
                    split_top,
                    split_bottom,
                    light.accent_fill.rgba(),
                    dark.accent_fill.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(left + px(24.), top + px(7.), px(44.), px(4.)),
                    99.,
                    split_top,
                    split_bottom,
                    light.text.rgba(),
                    dark.text.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(left + px(79.), top + px(7.5), px(30.), px(3.)),
                    99.,
                    split_top,
                    split_bottom,
                    light.muted.rgba(),
                    dark.muted.rgba(),
                );

                // Sidebar geometry is shared by both variants. The diagonal
                // changes its palette, never its structure.
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(left + px(7.), top + px(28.), px(22.), px(17.)),
                    4.,
                    split_top,
                    split_bottom,
                    light.sidebar_selected.rgba(),
                    dark.sidebar_selected.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(left + px(14.), top + px(33.), px(8.), px(8.)),
                    99.,
                    split_top,
                    split_bottom,
                    light.accent_fill.rgba(),
                    dark.accent_fill.rgba(),
                );
                for offset in [px(54.), px(65.)] {
                    Self::paint_split_preview_shape(
                        window,
                        Self::preview_bounds(left + px(11.), top + offset, px(14.), px(3.)),
                        99.,
                        split_top,
                        split_bottom,
                        light.sidebar_muted.rgba(),
                        dark.sidebar_muted.rgba(),
                    );
                }

                let title_width = px((content_width.as_f32() * 0.18).clamp(52., 76.));
                let subtitle_width = px((content_width.as_f32() * 0.31).clamp(88., 142.));
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(content_left, top + px(28.), title_width, px(5.)),
                    99.,
                    split_top,
                    split_bottom,
                    light.text.rgba(),
                    dark.text.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(content_left, top + px(39.), subtitle_width, px(3.)),
                    99.,
                    split_top,
                    split_bottom,
                    light.subtext.rgba(),
                    dark.subtext.rgba(),
                );

                let card_gap = px(8.);
                let primary_width = content_width * 0.61;
                let secondary_width = content_width - primary_width - card_gap;
                let primary_card =
                    Self::preview_bounds(content_left, top + px(51.), primary_width, px(29.));
                let secondary_card = Self::preview_bounds(
                    content_left + primary_width + card_gap,
                    top + px(51.),
                    secondary_width,
                    px(29.),
                );
                Self::paint_preview_card(
                    window,
                    primary_card,
                    split_top,
                    split_bottom,
                    light,
                    dark,
                );
                Self::paint_preview_card(
                    window,
                    secondary_card,
                    split_top,
                    split_bottom,
                    light,
                    dark,
                );

                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(
                        primary_card.origin.x + px(10.),
                        primary_card.origin.y + px(8.),
                        px(8.),
                        px(8.),
                    ),
                    99.,
                    split_top,
                    split_bottom,
                    light.accent_fill.rgba(),
                    dark.accent_fill.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(
                        primary_card.origin.x + px(25.),
                        primary_card.origin.y + px(8.),
                        px(48.),
                        px(4.),
                    ),
                    99.,
                    split_top,
                    split_bottom,
                    light.text.rgba(),
                    dark.text.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(
                        primary_card.origin.x + px(25.),
                        primary_card.origin.y + px(17.),
                        px(76.),
                        px(3.),
                    ),
                    99.,
                    split_top,
                    split_bottom,
                    light.muted.rgba(),
                    dark.muted.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(
                        primary_card.origin.x + primary_card.size.width - px(39.),
                        primary_card.origin.y + px(8.),
                        px(29.),
                        px(11.),
                    ),
                    99.,
                    split_top,
                    split_bottom,
                    light.accent_fill.rgba(),
                    dark.accent_fill.rgba(),
                );

                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(
                        secondary_card.origin.x + px(10.),
                        secondary_card.origin.y + px(8.),
                        px((secondary_card.size.width.as_f32() * 0.35).clamp(30., 48.)),
                        px(4.),
                    ),
                    99.,
                    split_top,
                    split_bottom,
                    light.subtext.rgba(),
                    dark.subtext.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(
                        secondary_card.origin.x + secondary_card.size.width - px(18.),
                        secondary_card.origin.y + px(7.),
                        px(7.),
                        px(7.),
                    ),
                    99.,
                    split_top,
                    split_bottom,
                    light.green.rgba(),
                    dark.green.rgba(),
                );
                let progress_width = secondary_card.size.width - px(20.);
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(
                        secondary_card.origin.x + px(10.),
                        secondary_card.origin.y + px(19.),
                        progress_width,
                        px(3.),
                    ),
                    99.,
                    split_top,
                    split_bottom,
                    light.inset.rgba(),
                    dark.inset.rgba(),
                );
                Self::paint_split_preview_shape(
                    window,
                    Self::preview_bounds(
                        secondary_card.origin.x + px(10.),
                        secondary_card.origin.y + px(19.),
                        progress_width * 0.58,
                        px(3.),
                    ),
                    99.,
                    split_top,
                    split_bottom,
                    light.accent_fill.rgba(),
                    dark.accent_fill.rgba(),
                );
            },
        )
        .w_full()
        .h(px(92.))
    }

    fn render_theme_card(&self, record: &ThemeRecord, cx: &mut Context<Self>) -> gpui::AnyElement {
        let family = record.family.clone();
        let family_id = family.id.clone();
        let selected = self.selected_family == family_id;
        let built_in = record.built_in;
        let apply_id = family_id.clone();
        let duplicate_id = family_id.clone();
        let edit_id = family_id.clone();
        let export_id = family_id.clone();
        let delete_id = family_id.clone();

        let mut actions = div().flex().flex_row().items_center().flex_wrap().gap_2();
        if !selected {
            actions = actions.child(
                components::button(
                    SharedString::from(format!("theme-apply-{family_id}")),
                    "应用",
                    ButtonTone::Primary,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(move |this, _event, window, cx| {
                    this.apply_selection(apply_id.clone(), this.mode, window, cx);
                })),
            );
        }
        if built_in {
            actions = actions.child(
                components::button(
                    SharedString::from(format!("theme-copy-{family_id}")),
                    "复制并编辑",
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.duplicate_and_edit(&duplicate_id, cx);
                })),
            );
        } else {
            actions = actions
                .child(
                    components::button(
                        SharedString::from(format!("theme-edit-{family_id}")),
                        "编辑",
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.open_editor(&edit_id, cx);
                    })),
                )
                .child(
                    components::button(
                        SharedString::from(format!("theme-delete-{family_id}")),
                        "删除",
                        ButtonTone::Danger,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.confirm_delete = Some(delete_id.clone());
                        cx.notify();
                    })),
                );
        }
        actions = actions.child(
            components::button(
                SharedString::from(format!("theme-export-{family_id}")),
                "导出",
                ButtonTone::Ghost,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.export_theme(&export_id, cx);
            })),
        );

        div()
            .flex()
            .flex_col()
            .min_w_0()
            .overflow_hidden()
            .rounded_lg()
            .border_1()
            .border_color(if selected {
                theme::accent()
            } else {
                theme::border()
            })
            .bg(theme::surface())
            .when(selected, |card| card.shadow(theme::shadow_hover()))
            .child(Self::pair_preview(&family))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_color(theme::text())
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(SharedString::from(family.name.clone())),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::muted())
                                            .child(SharedString::from(family.description.clone())),
                                    ),
                            )
                            .child(if selected {
                                components::badge(BadgeTone::Accent, "正在使用")
                            } else if built_in {
                                components::badge(BadgeTone::Neutral, "内置")
                            } else {
                                components::badge(BadgeTone::Teal, "用户")
                            }),
                    )
                    .child(actions),
            )
            .into_any_element()
    }

    fn render_manager(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mode_listener = cx.listener(|this, index: &usize, window, cx| {
            let mode = match *index {
                1 => ThemeMode::Light,
                2 => ThemeMode::Dark,
                _ => ThemeMode::System,
            };
            this.set_mode(mode, window, cx);
        });
        let mode_control = components::segmented(
            "theme-mode",
            &["跟随系统", "固定浅色", "固定深色"],
            Self::mode_index(self.mode),
            move |index, window, cx| mode_listener(&index, window, cx),
        );

        let cards = self
            .registry
            .themes
            .iter()
            .map(|record| self.render_theme_card(record, cx))
            .collect::<Vec<_>>();
        let content = layout::wide_column()
            .child(layout::section_header(
                "显示方式",
                "每套配色都同时包含浅色与深色；跟随系统时会在同一套配色内自动切换。",
            ))
            .child(
                components::card()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(layout::row_label(
                        "主题外观",
                        "固定模式用于始终使用指定的浅色或深色外观。",
                    ))
                    .child(mode_control),
            )
            .child(layout::section_header(
                "主题库",
                "色板样图同时展示每套配色的浅色和深色界面。",
            ))
            .child(
                div()
                    .grid()
                    .grid_cols(2)
                    .items_start()
                    .gap_3()
                    .w_full()
                    .children(cards),
            );

        layout::page()
            .relative()
            .child(
                layout::page_header("主题", Some("管理、预览与分享完整的深浅配色。".into())).child(
                    components::icon_button_tone(
                        "theme-import",
                        "导入主题",
                        IconName::Archive,
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.import_theme(cx);
                    })),
                ),
            )
            .child(layout::scroll_body("theme-manager-body", content))
            .when_some(self.confirm_delete.clone(), |root, family_id| {
                let family_name = self
                    .registry
                    .themes
                    .iter()
                    .find(|record| record.family.id == family_id)
                    .map(|record| record.family.name.clone())
                    .unwrap_or_else(|| family_id.clone());
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header("删除主题"))
                        .child(
                            components::modal_body().child(
                                div().text_sm().text_color(theme::subtext()).child(
                                    SharedString::from(format!(
                                    "确定删除用户主题「{family_name}」吗？主题文件将从本机移除。"
                                )),
                                ),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "theme-delete-cancel",
                                "取消",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm_delete = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "theme-delete-confirm",
                                "删除",
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.delete_confirmed(window, cx);
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
            .into_any_element()
    }

    fn render_color_row(
        descriptor_index: usize,
        palette: Theme,
        input: Entity<TextInput>,
    ) -> gpui::AnyElement {
        let descriptor = &THEME_TOKENS[descriptor_index];
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .w_full()
            .px_4()
            .py_2()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::text())
                            .child(descriptor.label),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(descriptor.key),
                    ),
            )
            .child(
                div()
                    .size(px(28.))
                    .rounded_md()
                    .border_1()
                    .border_color(theme::border_strong())
                    .bg(palette.color(descriptor.token).rgba()),
            )
            .child(div().w(px(150.)).child(input))
            .into_any_element()
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(editor) = self.editor.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let variant_listener = cx.listener(|this, index: &usize, _window, cx| {
            if let Some(editor) = this.editor.as_mut() {
                editor.variant = if *index == 0 {
                    EditorVariant::Light
                } else {
                    EditorVariant::Dark
                };
                cx.notify();
            }
        });
        let variant_index = usize::from(editor.variant == EditorVariant::Dark);
        let palette = if editor.variant == EditorVariant::Light {
            editor.family.light
        } else {
            editor.family.dark
        };
        let inputs = if editor.variant == EditorVariant::Light {
            &editor.light_colors
        } else {
            &editor.dark_colors
        };

        let mut token_sections = div().flex().flex_col().gap_3().w_full();
        for group in ["表面", "文字与边框", "强调与选中", "状态", "效果"] {
            let rows = THEME_TOKENS
                .iter()
                .enumerate()
                .filter(|(_, descriptor)| descriptor.group == group)
                .map(|(index, _)| Self::render_color_row(index, palette, inputs[index].clone()))
                .collect::<Vec<_>>();
            token_sections = token_sections
                .child(layout::section_header(
                    SharedString::from(group.to_string()),
                    SharedString::from(format!("{group}相关语义颜色。")),
                ))
                .child(layout::group(rows));
        }

        let content = layout::wide_column()
            .child(layout::section_header(
                "配色样图",
                "更新预览后，这里和整个应用都会使用当前草稿。",
            ))
            .child(
                div()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::border())
                    .overflow_hidden()
                    .child(Self::pair_preview(&editor.family)),
            )
            .child(layout::section_header(
                "主题信息",
                "主题 ID 保持稳定，名称、作者和说明可自由修改。",
            ))
            .child(
                components::card()
                    .gap_3()
                    .child(components::field(
                        "主题名称",
                        true,
                        None,
                        editor.name.clone(),
                    ))
                    .child(components::field(
                        "作者",
                        false,
                        None,
                        editor.author.clone(),
                    ))
                    .child(components::field(
                        "说明",
                        false,
                        None,
                        editor.description.clone(),
                    )),
            )
            .child(layout::section_header(
                "颜色令牌",
                "浅色与深色必须分别完整配置并通过可读性校验。",
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .child(components::segmented(
                        "theme-editor-variant",
                        &["浅色", "深色"],
                        variant_index,
                        move |index, window, cx| variant_listener(&index, window, cx),
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child("HEX 格式：#RRGGBB"),
                    ),
            )
            .child(token_sections);

        layout::page()
            .child(
                layout::page_header(
                    SharedString::from(format!("编辑 {}", editor.family.name)),
                    Some("内置主题的副本可以安全修改、导出和分享。".into()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            components::button(
                                "theme-editor-cancel",
                                "取消",
                                ButtonTone::Ghost,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, window, cx| {
                                    this.cancel_editor(window, cx);
                                },
                            )),
                        )
                        .child(
                            components::button(
                                "theme-editor-preview",
                                "更新预览",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.preview_editor(cx);
                                },
                            )),
                        )
                        .child(
                            components::button(
                                "theme-editor-save",
                                "保存并应用",
                                ButtonTone::Primary,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, window, cx| {
                                    this.save_editor(window, cx);
                                },
                            )),
                        ),
                ),
            )
            .child(layout::scroll_body("theme-editor-body", content))
            .into_any_element()
    }
}

impl Render for ThemeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.editor.is_some() {
            self.render_editor(cx)
        } else {
            self.render_manager(cx)
        }
    }
}

crate::notifications::impl_status_toasts!(ThemeView);

#[cfg(test)]
mod tests {
    use gpui::{point, px, size, Bounds};

    use super::ThemeView;

    #[test]
    fn preview_top_mask_rounds_only_the_outer_corners() {
        let bounds = Bounds {
            origin: point(px(10.), px(20.)),
            size: size(px(100.), px(40.)),
        };
        let points = ThemeView::preview_top_rounded_polygon(bounds, 8.);

        assert_eq!(points.first(), Some(&point(px(18.), px(20.))));
        assert_eq!(points.get(1), Some(&point(px(102.), px(20.))));
        assert_eq!(points.get(5), Some(&point(px(110.), px(28.))));
        assert_eq!(points.get(6), Some(&point(px(110.), px(60.))));
        assert_eq!(points.get(7), Some(&point(px(10.), px(60.))));
        assert_eq!(points.get(8), Some(&point(px(10.), px(28.))));
        assert_eq!(points.last(), Some(&point(px(18.), px(20.))));
        assert!(!points.contains(&point(px(10.), px(20.))));
        assert!(!points.contains(&point(px(110.), px(20.))));
    }
}

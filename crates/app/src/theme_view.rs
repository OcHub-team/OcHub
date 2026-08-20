//! Theme family library and editor.
//!
//! Each family owns a complete light/dark pair. Built-ins are immutable;
//! duplicating one creates an editable user file in `~/.ochub/themes/`.

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    App, Background, Bounds, ColorSpace, Context, Entity, ListAlignment, ListState, PathBuilder,
    PathPromptOptions, Pixels, Point, Rgba, SharedString, Window, canvas, div, linear_color_stop,
    linear_gradient, point, prelude::*, px, size,
};
use ochub_core::settings::{self, ThemeMode};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::icons::IconName;
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::product_ui::{common as common_screen, themes as theme_screen};
use crate::text_input::TextInput;
use crate::tf;
use crate::theme::{
    self, MAX_SURFACE_OPACITY_PERCENT, MIN_SURFACE_OPACITY_PERCENT, THEME_TOKENS, Theme,
    ThemeColor, ThemeFamily, ThemeRecord, ThemeWindowBackground,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorVariant {
    Light,
    Dark,
}

impl EditorVariant {
    fn current() -> Self {
        if theme::is_dark() {
            Self::Dark
        } else {
            Self::Light
        }
    }

    fn from_index(index: usize) -> Self {
        if index == 0 { Self::Light } else { Self::Dark }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Light => raw(k::THEME_EDITOR_VARIANT_LIGHT),
            Self::Dark => raw(k::THEME_EDITOR_VARIANT_DARK),
        }
    }

    fn palette(self, family: &ThemeFamily) -> Theme {
        match self {
            Self::Light => family.light,
            Self::Dark => family.dark,
        }
    }

    fn palette_mut(self, family: &mut ThemeFamily) -> &mut Theme {
        match self {
            Self::Light => &mut family.light,
            Self::Dark => &mut family.dark,
        }
    }
}

struct ThemeEffectInputs {
    sidebar_opacity: Option<Entity<TextInput>>,
    content_opacity: Option<Entity<TextInput>>,
}

struct ThemeVariantInputs {
    effects: ThemeEffectInputs,
    colors: Vec<Option<Entity<TextInput>>>,
}

impl ThemeVariantInputs {
    fn empty() -> Self {
        Self {
            effects: ThemeEffectInputs {
                sidebar_opacity: None,
                content_opacity: None,
            },
            colors: (0..THEME_TOKENS.len()).map(|_| None).collect(),
        }
    }
}

struct ThemeEditor {
    family: ThemeFamily,
    variant: EditorVariant,
    name: Entity<TextInput>,
    author: Entity<TextInput>,
    description: Entity<TextInput>,
    variant_inputs: ThemeVariantInputs,
}

#[derive(Clone, Copy)]
enum ThemeEditorBlock {
    Preview,
    Information,
    Variant,
    Material,
    /// A colour-token group, named by the identity `ThemeTokenDescriptor::group`
    /// carries. It is matched with `==`, never rendered — `token_group_header`
    /// maps it to the text a user sees.
    TokenGroup(&'static str),
}

const THEME_EDITOR_BLOCKS: &[ThemeEditorBlock] = &[
    ThemeEditorBlock::Preview,
    ThemeEditorBlock::Information,
    ThemeEditorBlock::Variant,
    ThemeEditorBlock::Material,
    ThemeEditorBlock::TokenGroup("表面"),
    ThemeEditorBlock::TokenGroup("文字与边框"),
    ThemeEditorBlock::TokenGroup("强调与选中"),
    ThemeEditorBlock::TokenGroup("状态"),
    ThemeEditorBlock::TokenGroup("效果"),
];

pub struct ThemeView {
    registry: Arc<theme::ThemeRegistry>,
    selected_family: String,
    mode: ThemeMode,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
    editor: Option<ThemeEditor>,
    editor_list_state: ListState,
    manager_list_state: ListState,
    confirm_delete: Option<String>,
    io_busy: bool,
}

impl ThemeView {
    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// `refresh_windows` re-runs `render`, but gpui's virtualized lists cache
    /// measured item heights and invalidate them only on a width change, so a
    /// translation that changes a row's height would otherwise leave the list
    /// scrolled to stale offsets.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        // Placeholders are captured when an input is constructed, so an editor
        // that is already open needs them pushed in by hand. The opacity and
        // colour fields hint at a format (`0–100%`, `#RRGGBB`) rather than at
        // prose, so they are the same in every language.
        if let Some(editor) = self.editor.as_ref() {
            let (name, author, description) = (
                editor.name.clone(),
                editor.author.clone(),
                editor.description.clone(),
            );
            name.update(cx, |input, cx| {
                input.set_placeholder(t(k::THEME_EDITOR_INFO_NAME_PLACEHOLDER), cx)
            });
            author.update(cx, |input, cx| {
                input.set_placeholder(t(k::THEME_EDITOR_INFO_AUTHOR_PLACEHOLDER), cx)
            });
            description.update(cx, |input, cx| {
                input.set_placeholder(t(k::THEME_EDITOR_INFO_DESCRIPTION_PLACEHOLDER), cx)
            });
        }
        self.editor_list_state.remeasure();
        self.manager_list_state.remeasure();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let registry = cx
                .background_spawn(async { theme::reload_registry() })
                .await;
            this.update(cx, |this, cx| {
                this.registry = registry;
                this.reset_manager_list();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

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
            SharedString::from(tf!(
                k::THEME_STATUS_DIAGNOSTICS,
                count = registry.diagnostics.len()
            ))
        });
        let status_level = status.is_some().then_some(NotificationLevel::Warning);
        let manager_item_count = Self::manager_item_count(registry.themes.len());
        Self {
            registry,
            selected_family,
            mode: settings.theme_mode,
            status,
            status_level,
            editor: None,
            editor_list_state: ListState::new(
                THEME_EDITOR_BLOCKS.len(),
                ListAlignment::Top,
                px(560.),
            ),
            manager_list_state: ListState::new(manager_item_count, ListAlignment::Top, px(720.)),
            confirm_delete: None,
            io_busy: false,
        }
    }

    fn manager_item_count(theme_count: usize) -> usize {
        1 + theme_count.div_ceil(2)
    }

    fn reset_manager_list(&self) {
        self.manager_list_state
            .reset(Self::manager_item_count(self.registry.themes.len()));
    }

    /// Every status carries its own severity; nothing is left to keyword
    /// inference, which cannot survive translation.
    fn set_status(
        &mut self,
        level: NotificationLevel,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(message.into());
        self.status_level = Some(level);
        cx.notify();
    }

    /// Drop a pending status without emitting a toast, keeping the level in
    /// step so it can never leak onto the next message.
    fn clear_status(&mut self) {
        self.status = None;
        self.status_level = None;
    }

    fn persist_selection(
        &mut self,
        family: String,
        mode: ThemeMode,
        success: SharedString,
        cx: &mut Context<Self>,
    ) {
        if self.io_busy {
            return;
        }
        self.io_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    settings::mutate_settings(move |settings| {
                        settings.theme_family = family;
                        settings.theme_mode = mode;
                    })
                    .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.io_busy = false;
                match result {
                    Ok(()) => this.set_status(NotificationLevel::Success, success, cx),
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::THEME_STATUS_SELECTION_SAVE_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    fn apply_selection(
        &mut self,
        family_id: String,
        mode: ThemeMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.io_busy {
            return;
        }
        let Some(family) = self
            .registry
            .themes
            .iter()
            .find(|record| record.family.id == family_id)
            .map(|record| record.family.clone())
        else {
            self.set_status(NotificationLevel::Error, t(k::THEME_STATUS_MISSING), cx);
            return;
        };

        self.selected_family = family_id;
        self.mode = mode;
        theme::install_family(&family, crate::ui_theme_mode(mode), window.appearance());
        theme::apply_window_background(window);
        cx.refresh_windows();
        self.persist_selection(
            self.selected_family.clone(),
            mode,
            SharedString::from(tf!(k::THEME_STATUS_APPLIED, name = family.name)),
            cx,
        );
    }

    fn set_mode(&mut self, mode: ThemeMode, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_selection(self.selected_family.clone(), mode, window, cx);
    }

    fn restore_saved_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = settings::get_settings();
        theme::install_selected(
            &settings.theme_family,
            crate::ui_theme_mode(settings.theme_mode),
            window.appearance(),
        );
        theme::apply_window_background(window);
        cx.refresh_windows();
    }

    fn make_input(
        cx: &mut Context<Self>,
        placeholder: &'static str,
        value: String,
    ) -> Entity<TextInput> {
        cx.new(move |cx| TextInput::new(cx, placeholder).with_content(value))
    }

    fn make_editor(&self, family: ThemeFamily, cx: &mut Context<Self>) -> ThemeEditor {
        let variant = EditorVariant::current();
        let name = Self::make_input(
            cx,
            raw(k::THEME_EDITOR_INFO_NAME_PLACEHOLDER),
            family.name.clone(),
        );
        let author = Self::make_input(
            cx,
            raw(k::THEME_EDITOR_INFO_AUTHOR_PLACEHOLDER),
            family.author.clone(),
        );
        let description = Self::make_input(
            cx,
            raw(k::THEME_EDITOR_INFO_DESCRIPTION_PLACEHOLDER),
            family.description.clone(),
        );
        ThemeEditor {
            family,
            variant,
            name,
            author,
            description,
            variant_inputs: ThemeVariantInputs::empty(),
        }
    }

    fn ensure_effect_inputs(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(Entity<TextInput>, Entity<TextInput>)> {
        let editor = self.editor.as_mut()?;
        let palette = editor.variant.palette(&editor.family);
        let sidebar_opacity = editor
            .variant_inputs
            .effects
            .sidebar_opacity
            .get_or_insert_with(|| {
                Self::make_input(cx, "0–100%", palette.effects.sidebar_opacity.to_string())
            })
            .clone();
        let content_opacity = editor
            .variant_inputs
            .effects
            .content_opacity
            .get_or_insert_with(|| {
                Self::make_input(cx, "0–100%", palette.effects.content_opacity.to_string())
            })
            .clone();
        Some((sidebar_opacity, content_opacity))
    }

    fn ensure_color_input(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TextInput>> {
        let editor = self.editor.as_mut()?;
        let descriptor = THEME_TOKENS.get(index)?;
        let palette = editor.variant.palette(&editor.family);
        let input = editor.variant_inputs.colors.get_mut(index)?;
        Some(
            input
                .get_or_insert_with(|| {
                    Self::make_input(cx, "#RRGGBB", palette.color(descriptor.token).hex())
                })
                .clone(),
        )
    }

    fn open_editor(&mut self, family_id: &str, cx: &mut Context<Self>) {
        let Some(record) = self
            .registry
            .themes
            .iter()
            .find(|record| record.family.id == family_id)
            .cloned()
        else {
            self.set_status(
                NotificationLevel::Error,
                t(k::THEME_STATUS_EDIT_MISSING),
                cx,
            );
            return;
        };
        if record.built_in {
            self.set_status(
                NotificationLevel::Warning,
                t(k::THEME_STATUS_BUILT_IN_READ_ONLY),
                cx,
            );
            return;
        }
        self.editor = Some(self.make_editor(record.family, cx));
        self.editor_list_state.remeasure();
        self.clear_status();
        cx.notify();
    }

    fn input_value(input: &Entity<TextInput>, cx: &mut Context<Self>) -> String {
        input.read(cx).content().trim().to_string()
    }

    fn opacity_value(input: &Entity<TextInput>, label: &str, cx: &mut Context<Self>) -> Result<u8> {
        let value = Self::input_value(input, cx);
        let value = value.trim_end_matches('%').trim();
        let parsed = value
            .parse::<u8>()
            .with_context(|| tf!(k::THEME_ERROR_OPACITY_INTEGER, field = label))?;
        if !(MIN_SURFACE_OPACITY_PERCENT..=MAX_SURFACE_OPACITY_PERCENT).contains(&parsed) {
            return Err(anyhow!(tf!(
                k::THEME_ERROR_OPACITY_RANGE,
                field = label,
                min = MIN_SURFACE_OPACITY_PERCENT,
                max = MAX_SURFACE_OPACITY_PERCENT,
            )));
        }
        Ok(parsed)
    }

    fn sync_editor(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let editor = self
            .editor
            .as_mut()
            .ok_or_else(|| anyhow!(raw(k::THEME_ERROR_NO_EDITOR)))?;
        editor.family.name = Self::input_value(&editor.name, cx);
        editor.family.author = Self::input_value(&editor.author, cx);
        editor.family.description = Self::input_value(&editor.description, cx);
        let label = editor.variant.label();
        let sidebar_opacity = editor
            .variant_inputs
            .effects
            .sidebar_opacity
            .as_ref()
            .map(|input| {
                Self::opacity_value(
                    input,
                    &tf!(
                        k::THEME_ERROR_FIELD_QUALIFIED,
                        variant = label,
                        field = raw(k::THEME_EDITOR_MATERIAL_SIDEBAR_OPACITY_LABEL),
                    ),
                    cx,
                )
            })
            .transpose()?;
        let content_opacity = editor
            .variant_inputs
            .effects
            .content_opacity
            .as_ref()
            .map(|input| {
                Self::opacity_value(
                    input,
                    &tf!(
                        k::THEME_ERROR_FIELD_QUALIFIED,
                        variant = label,
                        field = raw(k::THEME_EDITOR_MATERIAL_CONTENT_OPACITY_LABEL),
                    ),
                    cx,
                )
            })
            .transpose()?;
        let colors = THEME_TOKENS
            .iter()
            .enumerate()
            .filter_map(|(index, descriptor)| {
                editor.variant_inputs.colors[index].as_ref().map(|input| {
                    ThemeColor::parse(&Self::input_value(input, cx))
                        .with_context(|| {
                            tf!(
                                k::THEME_ERROR_FIELD_QUALIFIED,
                                variant = label,
                                field = ochub_ui::i18n::raw(descriptor.label),
                            )
                        })
                        .map(|color| (descriptor.token, color))
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let palette = editor.variant.palette_mut(&mut editor.family);
        if let Some(sidebar_opacity) = sidebar_opacity {
            palette.effects.sidebar_opacity = sidebar_opacity;
        }
        if let Some(content_opacity) = content_opacity {
            palette.effects.content_opacity = content_opacity;
        }
        for (token, color) in colors {
            palette.set_color(token, color);
        }
        theme::validate_family(&editor.family)
    }

    fn switch_editor_variant(
        &mut self,
        variant: EditorVariant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .editor
            .as_ref()
            .is_none_or(|editor| editor.variant == variant)
        {
            return;
        }
        if let Err(err) = self.sync_editor(cx) {
            self.set_status(
                NotificationLevel::Error,
                tf!(k::THEME_STATUS_VARIANT_SWITCH_FAILED, error = err),
                cx,
            );
            return;
        }
        if let Some(editor) = self.editor.as_mut() {
            editor.variant = variant;
            editor.variant_inputs = ThemeVariantInputs::empty();
        }
        self.editor_list_state.remeasure();
        self.preview_editor(window, cx);
    }

    fn preview_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(err) = self.sync_editor(cx) {
            self.set_status(
                NotificationLevel::Error,
                tf!(k::THEME_STATUS_PREVIEW_FAILED, error = err),
                cx,
            );
            return;
        }
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let palette = editor.variant.palette(&editor.family);
        let dark = editor.variant == EditorVariant::Dark;
        theme::install_preview(palette, dark);
        theme::apply_window_background(window);
        cx.refresh_windows();
        self.set_status(NotificationLevel::Info, t(k::THEME_STATUS_PREVIEWING), cx);
    }

    fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor = None;
        self.restore_saved_theme(window, cx);
        self.clear_status();
        cx.notify();
    }

    fn save_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.io_busy {
            return;
        }
        if let Err(err) = self.sync_editor(cx) {
            self.set_status(
                NotificationLevel::Error,
                tf!(k::THEME_STATUS_SAVE_FAILED, error = err),
                cx,
            );
            return;
        }
        let Some(family) = self.editor.as_ref().map(|editor| editor.family.clone()) else {
            return;
        };
        let family_for_work = family.clone();
        let family_id = family.id.clone();
        let family_name = family.name.clone();
        let mode = self.mode;
        let appearance = window.appearance();
        self.io_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    theme::save_user_family(&family_for_work).map_err(|error| error.to_string())?;
                    let selected_family = family_for_work.id.clone();
                    settings::mutate_settings(move |settings| {
                        settings.theme_family = selected_family;
                        settings.theme_mode = mode;
                    })
                    .map_err(|error| error.to_string())?;
                    Ok::<_, String>(theme::reload_registry())
                })
                .await;
            this.update(cx, |this, cx| {
                this.io_busy = false;
                match result {
                    Ok(registry) => {
                        this.registry = registry;
                        this.reset_manager_list();
                        this.editor = None;
                        this.selected_family = family_id;
                        theme::install_family(&family, crate::ui_theme_mode(mode), appearance);
                        apply_theme_windows(cx);
                        this.set_status(
                            NotificationLevel::Success,
                            tf!(k::THEME_STATUS_SAVED, name = family_name),
                            cx,
                        );
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::THEME_STATUS_SAVE_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    fn duplicate_and_edit(&mut self, family_id: &str, cx: &mut Context<Self>) {
        if self.io_busy {
            return;
        }
        let Some(source) = self
            .registry
            .themes
            .iter()
            .find(|record| record.family.id == family_id)
            .map(|record| record.family.clone())
        else {
            self.set_status(
                NotificationLevel::Error,
                t(k::THEME_STATUS_DUPLICATE_MISSING),
                cx,
            );
            return;
        };
        match theme::duplicate_family(&source) {
            Ok(family) => {
                self.editor = Some(self.make_editor(family, cx));
                self.editor_list_state.remeasure();
                self.clear_status();
                cx.notify();
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::THEME_STATUS_DUPLICATE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn import_theme(&mut self, cx: &mut Context<Self>) {
        if self.io_busy {
            return;
        }
        self.io_busy = true;
        cx.notify();
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t(k::THEME_IMPORT_PROMPT)),
        });
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let Some(path) = path else {
                this.update(cx, |this, cx| {
                    this.io_busy = false;
                    cx.notify();
                })
                .ok();
                return;
            };
            let result = cx
                .background_spawn(async move {
                    let family = theme::import_family(&path).map_err(|error| error.to_string())?;
                    Ok::<_, String>((family, theme::reload_registry()))
                })
                .await;
            this.update(cx, |this, cx| {
                this.io_busy = false;
                match result {
                    Ok((family, registry)) => {
                        this.registry = registry;
                        this.reset_manager_list();
                        this.set_status(
                            NotificationLevel::Success,
                            tf!(k::THEME_STATUS_IMPORTED, name = family.name),
                            cx,
                        );
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::THEME_STATUS_IMPORT_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    fn export_theme(&mut self, family_id: &str, cx: &mut Context<Self>) {
        if self.io_busy {
            return;
        }
        let Some(family) = self
            .registry
            .themes
            .iter()
            .find(|record| record.family.id == family_id)
            .map(|record| record.family.clone())
        else {
            self.set_status(
                NotificationLevel::Error,
                t(k::THEME_STATUS_EXPORT_MISSING),
                cx,
            );
            return;
        };
        let directory = ochub_core::paths::get_app_config_dir();
        let suggested_name = format!("{}.ochub-theme.json", family.id);
        self.io_busy = true;
        cx.notify();
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested_name));
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(path))) => Some(path),
                _ => None,
            };
            let Some(path) = path else {
                this.update(cx, |this, cx| {
                    this.io_busy = false;
                    cx.notify();
                })
                .ok();
                return;
            };
            let display_path = path.display().to_string();
            let result = cx
                .background_spawn(async move { theme::export_family(&family, &path) })
                .await;
            this.update(cx, |this, cx| {
                this.io_busy = false;
                match result {
                    Ok(()) => this.set_status(
                        NotificationLevel::Success,
                        tf!(k::THEME_STATUS_EXPORTED, path = display_path),
                        cx,
                    ),
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::THEME_STATUS_EXPORT_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    fn delete_confirmed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.io_busy {
            return;
        }
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
            self.set_status(
                NotificationLevel::Warning,
                t(k::THEME_STATUS_DELETE_MISSING),
                cx,
            );
            return;
        };
        let was_selected = self.selected_family == family_id;
        let family_name = record.family.name.clone();
        let mode = self.mode;
        let appearance = window.appearance();
        self.io_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    theme::delete_user_family(&record).map_err(|error| error.to_string())?;
                    if was_selected {
                        settings::mutate_settings(|settings| {
                            settings.theme_family = theme::DEFAULT_THEME_FAMILY.to_string();
                        })
                        .map_err(|error| error.to_string())?;
                    }
                    Ok::<_, String>(theme::reload_registry())
                })
                .await;
            this.update(cx, |this, cx| {
                this.io_busy = false;
                match result {
                    Ok(registry) => {
                        this.registry = registry;
                        this.reset_manager_list();
                        if was_selected {
                            this.selected_family = theme::DEFAULT_THEME_FAMILY.to_string();
                            theme::install_selected(
                                &this.selected_family,
                                crate::ui_theme_mode(mode),
                                appearance,
                            );
                            apply_theme_windows(cx);
                        }
                        this.set_status(
                            NotificationLevel::Success,
                            tf!(k::THEME_STATUS_DELETED, name = family_name),
                            cx,
                        );
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::THEME_STATUS_DELETE_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
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

    fn pair_preview(family: &ThemeFamily) -> impl IntoElement + use<> {
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
                    t(k::THEME_CARD_APPLY),
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
                    t(k::THEME_CARD_DUPLICATE),
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
                        t(k::THEME_CARD_EDIT),
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
                        t(k::THEME_CARD_DELETE),
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
                t(k::THEME_CARD_EXPORT),
                ButtonTone::Ghost,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.export_theme(&export_id, cx);
            })),
        );

        let badge = if selected {
            components::badge(BadgeTone::Accent, t(k::THEME_CARD_BADGE_ACTIVE))
        } else if built_in {
            components::badge(BadgeTone::Neutral, t(k::THEME_CARD_BADGE_BUILT_IN))
        } else {
            components::badge(BadgeTone::Teal, t(k::THEME_CARD_BADGE_USER))
        };
        theme_screen::theme_card(theme_screen::ThemeCard {
            preview: Self::pair_preview(&family).into_any_element(),
            selected,
            name: SharedString::from(family.name.clone()),
            description: SharedString::from(family.description.clone()),
            badge: badge.into_any_element(),
            actions: actions.into_any_element(),
        })
        .into_any_element()
    }

    fn render_manager_item(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        if index == 0 {
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
                &[
                    raw(k::THEME_MODE_OPTION_SYSTEM),
                    raw(k::THEME_MODE_OPTION_LIGHT),
                    raw(k::THEME_MODE_OPTION_DARK),
                ],
                Self::mode_index(self.mode),
                move |index, window, cx| mode_listener(&index, window, cx),
            );
            return theme_screen::mode_block(
                t(k::THEME_MODE_SECTION_TITLE),
                t(k::THEME_MODE_ROW_LABEL),
                mode_control,
                t(k::THEME_LIBRARY_SECTION_TITLE),
            )
            .into_any_element();
        }

        let first = (index - 1) * 2;
        let mut cards = Vec::new();
        for record in self.registry.themes.iter().skip(first).take(2) {
            cards.push(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_theme_card(record, cx))
                    .into_any_element(),
            );
        }
        theme_screen::card_row(cards, first + 1 >= self.registry.themes.len()).into_any_element()
    }

    fn render_manager(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let list = gpui::list(
            self.manager_list_state.clone(),
            cx.processor(|this, index: usize, _window, cx| this.render_manager_item(index, cx)),
        );

        common_screen::page(
            t(k::THEME_PAGE_TITLE),
            components::icon_button_tone(
                "theme-import",
                t(k::THEME_IMPORT_BUTTON),
                IconName::Archive,
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.import_theme(cx);
            })),
            layout::wide_virtual_body("theme-manager-body", list, &self.manager_list_state),
        )
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
                    .child(components::modal_header(t(k::THEME_DELETE_MODAL_TITLE)))
                    .child(
                        components::modal_body().child(
                            div()
                                .text_sm()
                                .text_color(theme::subtext())
                                .child(SharedString::from(tf!(
                                    k::THEME_DELETE_MODAL_BODY,
                                    name = family_name
                                ))),
                        ),
                    )
                    .child(components::modal_footer(vec![
                        components::button(
                            "theme-delete-cancel",
                            t(k::THEME_DELETE_MODAL_CANCEL),
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
                            t(k::THEME_DELETE_MODAL_CONFIRM),
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
                            .child(ochub_ui::i18n::t(descriptor.label)),
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

    fn render_opacity_input(input: Entity<TextInput>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap_2()
            .child(div().w(px(88.)).child(input))
            .child(div().text_sm().text_color(theme::muted()).child("%"))
    }

    /// Heading and blurb for a colour-token group.
    ///
    /// The group name is an identity — `descriptor.group` is matched against it
    /// with `==` — so it is never rendered directly; this maps it to text that
    /// follows the locale.
    fn token_group_title(group: &str) -> SharedString {
        match group {
            "文字与边框" => t(k::THEME_EDITOR_TOKENS_TEXT_TITLE),
            "强调与选中" => t(k::THEME_EDITOR_TOKENS_ACCENT_TITLE),
            "状态" => t(k::THEME_EDITOR_TOKENS_STATUS_TITLE),
            "效果" => t(k::THEME_EDITOR_TOKENS_EFFECT_TITLE),
            // "表面", and the unreachable rest: the groups reaching here all come
            // from `THEME_EDITOR_BLOCKS`.
            _ => t(k::THEME_EDITOR_TOKENS_SURFACE_TITLE),
        }
    }

    fn render_editor_block(
        &mut self,
        block: ThemeEditorBlock,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.editor.is_none() {
            return gpui::Empty.into_any_element();
        }
        let effect_inputs = match block {
            ThemeEditorBlock::Material => self.ensure_effect_inputs(cx),
            _ => None,
        };
        let color_inputs = match block {
            ThemeEditorBlock::TokenGroup(group) => Some(
                THEME_TOKENS
                    .iter()
                    .enumerate()
                    .filter(|(_, descriptor)| descriptor.group == group)
                    .filter_map(|(index, _)| {
                        self.ensure_color_input(index, cx)
                            .map(|input| (index, input))
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        let Some(editor) = self.editor.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let palette = editor.variant.palette(&editor.family);
        let shell = div().flex().flex_col().items_start().w_full().pb_3();

        match block {
            ThemeEditorBlock::Preview => shell
                .child(layout::section_header(
                    t(k::THEME_EDITOR_PREVIEW_SECTION_TITLE),
                    None,
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
                .into_any_element(),
            ThemeEditorBlock::Information => shell
                .child(layout::section_header(
                    t(k::THEME_EDITOR_INFO_SECTION_TITLE),
                    None,
                ))
                .child(
                    components::card()
                        .gap_3()
                        .child(components::field(
                            t(k::THEME_EDITOR_INFO_NAME_LABEL),
                            true,
                            None,
                            editor.name.clone(),
                        ))
                        .child(components::field(
                            t(k::THEME_EDITOR_INFO_AUTHOR_LABEL),
                            false,
                            None,
                            editor.author.clone(),
                        ))
                        .child(components::field(
                            t(k::THEME_EDITOR_INFO_DESCRIPTION_LABEL),
                            false,
                            None,
                            editor.description.clone(),
                        )),
                )
                .into_any_element(),
            ThemeEditorBlock::Variant => {
                let variant_listener = cx.listener(|this, index: &usize, window, cx| {
                    this.switch_editor_variant(EditorVariant::from_index(*index), window, cx);
                });
                let variant_index = usize::from(editor.variant == EditorVariant::Dark);
                shell
                    .child(layout::section_header(
                        t(k::THEME_EDITOR_VARIANT_SECTION_TITLE),
                        None,
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
                                &[
                                    raw(k::THEME_EDITOR_VARIANT_LIGHT),
                                    raw(k::THEME_EDITOR_VARIANT_DARK),
                                ],
                                variant_index,
                                move |index, window, cx| variant_listener(&index, window, cx),
                            ))
                            .child(div().text_xs().text_color(theme::muted()).child(
                                if editor.variant == EditorVariant::Light {
                                    t(k::THEME_EDITOR_VARIANT_EDITING_LIGHT)
                                } else {
                                    t(k::THEME_EDITOR_VARIANT_EDITING_DARK)
                                },
                            )),
                    )
                    .into_any_element()
            }
            ThemeEditorBlock::Material => {
                let background_listener = cx.listener(|this, index: &usize, window, cx| {
                    if let Some(editor) = this.editor.as_mut() {
                        let effect = if *index == 0 {
                            ThemeWindowBackground::Blurred
                        } else {
                            ThemeWindowBackground::Opaque
                        };
                        editor
                            .variant
                            .palette_mut(&mut editor.family)
                            .effects
                            .window_background = effect;
                    }
                    this.preview_editor(window, cx);
                });
                let background_index =
                    usize::from(palette.effects.window_background == ThemeWindowBackground::Opaque);
                let Some((sidebar_opacity, content_opacity)) = effect_inputs else {
                    return gpui::Empty.into_any_element();
                };
                shell
                    .child(layout::section_header(
                        t(k::THEME_EDITOR_MATERIAL_SECTION_TITLE),
                        None,
                    ))
                    .child(layout::group(vec![
                        components::field_row(
                            t(k::THEME_EDITOR_MATERIAL_WINDOW_BACKGROUND_LABEL),
                            None,
                            components::segmented(
                                "theme-editor-window-background",
                                &[
                                    raw(k::THEME_EDITOR_MATERIAL_WINDOW_BACKGROUND_BLURRED),
                                    raw(k::THEME_EDITOR_MATERIAL_WINDOW_BACKGROUND_OPAQUE),
                                ],
                                background_index,
                                move |index, window, cx| background_listener(&index, window, cx),
                            ),
                        )
                        .into_any_element(),
                        components::field_row(
                            t(k::THEME_EDITOR_MATERIAL_SIDEBAR_OPACITY_LABEL),
                            None,
                            Self::render_opacity_input(sidebar_opacity),
                        )
                        .into_any_element(),
                        components::field_row(
                            t(k::THEME_EDITOR_MATERIAL_CONTENT_OPACITY_LABEL),
                            None,
                            Self::render_opacity_input(content_opacity),
                        )
                        .into_any_element(),
                    ]))
                    .into_any_element()
            }
            ThemeEditorBlock::TokenGroup(group) => {
                let rows = color_inputs
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(index, input)| Self::render_color_row(index, palette, input))
                    .collect::<Vec<_>>();
                let group_title = Self::token_group_title(group);
                shell
                    .when(group == "表面", |section| {
                        section.child(layout::section_header(
                            t(k::THEME_EDITOR_TOKENS_SECTION_TITLE),
                            None,
                        ))
                    })
                    .child(layout::section_header(group_title, None))
                    .child(layout::group(rows))
                    .into_any_element()
            }
        }
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(editor) = self.editor.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let list = gpui::list(
            self.editor_list_state.clone(),
            cx.processor(|this, index: usize, _window, cx| {
                THEME_EDITOR_BLOCKS
                    .get(index)
                    .copied()
                    .map(|block| this.render_editor_block(block, cx))
                    .unwrap_or_else(|| gpui::Empty.into_any_element())
            }),
        );

        layout::page()
            .child(
                layout::page_header(
                    SharedString::from(tf!(k::THEME_EDITOR_TITLE, name = editor.family.name)),
                    None,
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
                                t(k::THEME_EDITOR_ACTION_CANCEL),
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
                                t(k::THEME_EDITOR_ACTION_PREVIEW),
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, window, cx| {
                                    this.preview_editor(window, cx);
                                },
                            )),
                        )
                        .child(
                            components::button(
                                "theme-editor-save",
                                t(k::THEME_EDITOR_ACTION_SAVE),
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
            .child(layout::wide_virtual_body(
                "theme-editor-body",
                list,
                &self.editor_list_state,
            ))
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

fn apply_theme_windows(cx: &mut App) {
    for window in cx.windows() {
        let _ = window.update(cx, |_root, window, _cx| {
            theme::apply_window_background(window);
        });
    }
    cx.refresh_windows();
}

crate::notifications::impl_status_toasts_leveled!(ThemeView);

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px, size};

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

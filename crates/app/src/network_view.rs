//! The Network page: an app-wide HTTP/SOCKS5 proxy for OcHub's own outbound
//! requests (update checks, model listing, balance/subscription lookups, ...).
//!
//! Deliberately out of scope: the Gateway's own reverse-proxy forwarding to
//! model providers, which is unrelated request traffic and keeps its own
//! `reqwest::Client`.
//!
//! One batched form, like [`crate::settings_view::sync`]: `enabled` and
//! `protocol` live in the same draft as the credential fields rather than
//! applying instantly, specifically so this page can end in one
//! [`components::commit_bar`] with no exception to "never pair a commit bar
//! with a row that applies immediately".

use gpui::{AnyElement, App, Context, Entity, ScrollHandle, SharedString, Window, div, prelude::*};
use ochub_core::settings::{self, AppSettings, ProxyProtocol, ProxySettings};

use crate::components::{self, ButtonTone};
use crate::i18n::{Key, k, raw, t};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::text_input::{TextInput, TextInputEvent};
use crate::tf;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldError {
    Required,
    Invalid,
}

pub struct NetworkView {
    settings: AppSettings,
    enabled: bool,
    protocol: ProxyProtocol,
    host_input: Entity<TextInput>,
    port_input: Entity<TextInput>,
    username_input: Entity<TextInput>,
    password_input: Entity<TextInput>,
    baseline_enabled: bool,
    baseline_protocol: ProxyProtocol,
    baseline_host: SharedString,
    baseline_port: SharedString,
    baseline_username: SharedString,
    baseline_password: SharedString,
    host_error: Option<FieldError>,
    port_error: Option<FieldError>,
    saving: bool,
    testing: bool,
    scroll: ScrollHandle,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
}

impl NetworkView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let settings = settings::get_settings();
        let proxy = settings.proxy.clone().unwrap_or_default();
        let port_text = port_to_string(proxy.port);

        let host_input = cx.new(|cx| {
            let mut input = TextInput::new(cx, SharedString::new_static("127.0.0.1"));
            input.set_content(proxy.host.clone(), cx);
            input
        });
        let port_input = cx.new(|cx| {
            let mut input = TextInput::new(cx, SharedString::new_static("1080"));
            input.set_content(port_text.clone(), cx);
            input
        });
        let username_input = cx.new(|cx| {
            let mut input = TextInput::new(cx, SharedString::default());
            input.set_content(proxy.username.clone(), cx);
            input
        });
        let password_input = cx.new(|cx| {
            let mut input = TextInput::new(cx, SharedString::default()).masked(true);
            input.set_content(proxy.password.clone(), cx);
            input
        });

        // A corrected field stops shouting immediately; validation itself
        // still runs on Save/Test, so a half-typed value is never an error.
        cx.subscribe(
            &host_input,
            |this: &mut Self, _input, _: &TextInputEvent, cx| {
                this.host_error = None;
                cx.notify();
            },
        )
        .detach();
        cx.subscribe(
            &port_input,
            |this: &mut Self, _input, _: &TextInputEvent, cx| {
                this.port_error = None;
                cx.notify();
            },
        )
        .detach();

        Self {
            settings,
            enabled: proxy.enabled,
            protocol: proxy.protocol,
            host_input,
            port_input,
            username_input,
            password_input,
            baseline_enabled: proxy.enabled,
            baseline_protocol: proxy.protocol,
            baseline_host: SharedString::from(proxy.host),
            baseline_port: SharedString::from(port_text),
            baseline_username: SharedString::from(proxy.username),
            baseline_password: SharedString::from(proxy.password),
            host_error: None,
            port_error: None,
            saving: false,
            testing: false,
            scroll: ScrollHandle::new(),
            status: None,
            status_level: None,
        }
    }

    /// Re-read after an external write (a language switch repaints every
    /// view). A dirty draft is left exactly as typed, matching the sync page.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.settings = settings::get_settings();
        if self.dirty(cx) {
            cx.notify();
            return;
        }
        self.reseed(cx);
    }

    fn reseed(&mut self, cx: &mut Context<Self>) {
        let proxy = self.settings.proxy.clone().unwrap_or_default();
        let port_text = port_to_string(proxy.port);
        self.host_input
            .update(cx, |input, cx| input.set_content(proxy.host.clone(), cx));
        self.port_input
            .update(cx, |input, cx| input.set_content(port_text.clone(), cx));
        self.username_input.update(cx, |input, cx| {
            input.set_content(proxy.username.clone(), cx)
        });
        self.password_input.update(cx, |input, cx| {
            input.set_content(proxy.password.clone(), cx)
        });
        self.enabled = proxy.enabled;
        self.protocol = proxy.protocol;
        self.baseline_enabled = proxy.enabled;
        self.baseline_protocol = proxy.protocol;
        self.baseline_host = SharedString::from(proxy.host);
        self.baseline_port = SharedString::from(port_text);
        self.baseline_username = SharedString::from(proxy.username);
        self.baseline_password = SharedString::from(proxy.password);
        self.host_error = None;
        self.port_error = None;
        cx.notify();
    }

    fn dirty(&self, cx: &App) -> bool {
        self.enabled != self.baseline_enabled
            || self.protocol != self.baseline_protocol
            || self.host_input.read(cx).content().trim() != self.baseline_host.trim()
            || self.port_input.read(cx).content().trim() != self.baseline_port.trim()
            || self.username_input.read(cx).content().trim() != self.baseline_username.trim()
            || self.password_input.read(cx).content().trim() != self.baseline_password.trim()
    }

    fn set_status(
        &mut self,
        level: NotificationLevel,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(text.into());
        self.status_level = Some(level);
        cx.notify();
    }

    fn discard(&mut self, cx: &mut Context<Self>) {
        let host = self.baseline_host.clone();
        let port = self.baseline_port.clone();
        let username = self.baseline_username.clone();
        let password = self.baseline_password.clone();
        self.host_input
            .update(cx, |input, cx| input.set_content(host, cx));
        self.port_input
            .update(cx, |input, cx| input.set_content(port, cx));
        self.username_input
            .update(cx, |input, cx| input.set_content(username, cx));
        self.password_input
            .update(cx, |input, cx| input.set_content(password, cx));
        self.enabled = self.baseline_enabled;
        self.protocol = self.baseline_protocol;
        self.host_error = None;
        self.port_error = None;
        cx.notify();
    }

    /// Host and port are always required: a proxy entry with neither is not a
    /// configuration, it's a typo waiting to be saved. `enabled` is what
    /// decides whether that configuration is actually *applied* — it does not
    /// relax this check, so flipping it on with nothing filled in still
    /// stops at Save rather than silently doing nothing.
    fn validated_candidate(&mut self, cx: &mut Context<Self>) -> Option<ProxySettings> {
        let host = self.host_input.read(cx).content().trim().to_string();
        let port_text = self.port_input.read(cx).content().trim().to_string();
        let username = self.username_input.read(cx).content().trim().to_string();
        let password = self.password_input.read(cx).content().trim().to_string();

        let host_error = host.is_empty().then_some(FieldError::Required);
        let (port, port_error) = if port_text.is_empty() {
            (0, Some(FieldError::Required))
        } else {
            match port_text.parse::<u16>() {
                Ok(value) if value > 0 => (value, None),
                _ => (0, Some(FieldError::Invalid)),
            }
        };

        if host_error.is_some() || port_error.is_some() {
            self.host_error = host_error;
            self.port_error = port_error;
            cx.notify();
            return None;
        }
        self.host_error = None;
        self.port_error = None;

        Some(ProxySettings {
            enabled: self.enabled,
            protocol: self.protocol,
            host,
            port,
            username,
            password,
        })
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some(mut candidate) = self.validated_candidate(cx) else {
            return;
        };
        candidate.normalize();
        self.saving = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    settings::mutate_settings(move |settings| {
                        settings.proxy = Some(candidate);
                    })
                    .map(|_| settings::get_settings())
                    .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(stored) => {
                        this.settings = stored;
                        this.reseed(cx);
                        this.set_status(
                            NotificationLevel::Success,
                            t(k::SETTINGS_STATUS_SAVED),
                            cx,
                        );
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_STATUS_SAVE_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Probes whatever is **typed**, without writing it anywhere — `enabled`
    /// is forced true for the probe regardless of the draft's toggle, since
    /// testing connectivity for a host/port is meaningful even before the
    /// user has committed to turning the proxy on for real traffic.
    fn test_connection(&mut self, cx: &mut Context<Self>) {
        if self.testing {
            return;
        }
        let Some(mut candidate) = self.validated_candidate(cx) else {
            return;
        };
        candidate.enabled = true;
        candidate.normalize();
        self.testing = true;
        self.set_status(NotificationLevel::Info, t(k::NETWORK_TEST_TESTING), cx);
        cx.spawn(async move |this, cx| {
            let outcome = crate::core_async::run(async move {
                ochub_core::services::network_proxy::check_connection(&candidate).await
            })
            .await;
            this.update(cx, |this, cx| {
                this.testing = false;
                match outcome {
                    Ok(()) => {
                        this.set_status(NotificationLevel::Success, t(k::NETWORK_TEST_OK), cx)
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::NETWORK_TEST_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    // ── Render ──────────────────────────────────────────────────────────────

    fn render_connection(&self, cx: &mut Context<Self>) -> gpui::Div {
        let protocol_index = match self.protocol {
            ProxyProtocol::Http => 0,
            ProxyProtocol::Socks5 => 1,
        };
        let toggle = cx.listener(|this: &mut Self, _: &(), _window, cx| {
            this.enabled = !this.enabled;
            cx.notify();
        });
        let protocol_select = cx.listener(|this: &mut Self, index: &usize, _window, cx| {
            this.protocol = if *index == 1 {
                ProxyProtocol::Socks5
            } else {
                ProxyProtocol::Http
            };
            cx.notify();
        });

        let card = components::card()
            .gap_4()
            .child(layout::switch_row(
                "network-proxy-enabled",
                t(k::NETWORK_FIELD_ENABLED_LABEL),
                t(k::NETWORK_FIELD_ENABLED_DESC),
                self.enabled,
                false,
                move |window, cx| toggle(&(), window, cx),
            ))
            .child(components::field_row(
                t(k::NETWORK_FIELD_PROTOCOL_LABEL),
                SharedString::default(),
                components::segmented(
                    "network-proxy-protocol",
                    &[
                        raw(k::NETWORK_PROTOCOL_OPTION_HTTP),
                        raw(k::NETWORK_PROTOCOL_OPTION_SOCKS5),
                    ],
                    protocol_index,
                    move |index, window, cx| protocol_select(&index, window, cx),
                ),
            ))
            .child(components::field_with_error(
                t(k::NETWORK_FIELD_HOST_LABEL),
                true,
                Some(t(k::NETWORK_FIELD_HOST_DESC)),
                self.host_error.map(|error| {
                    error_text(
                        error,
                        k::NETWORK_ERROR_HOST_REQUIRED,
                        k::NETWORK_ERROR_HOST_REQUIRED,
                    )
                }),
                div().w_full().child(self.host_input.clone()),
            ))
            .child(components::field_with_error(
                t(k::NETWORK_FIELD_PORT_LABEL),
                true,
                Some(t(k::NETWORK_FIELD_PORT_DESC)),
                self.port_error.map(|error| {
                    error_text(
                        error,
                        k::NETWORK_ERROR_PORT_REQUIRED,
                        k::NETWORK_ERROR_PORT_INVALID,
                    )
                }),
                div().w_full().child(self.port_input.clone()),
            ))
            .child(components::field(
                t(k::NETWORK_FIELD_USERNAME_LABEL),
                false,
                Some(t(k::NETWORK_FIELD_USERNAME_DESC)),
                div().w_full().child(self.username_input.clone()),
            ))
            .child(components::field(
                t(k::NETWORK_FIELD_PASSWORD_LABEL),
                false,
                Some(t(k::NETWORK_FIELD_PASSWORD_DESC)),
                div().w_full().child(self.password_input.clone()),
            ));
        section(
            t(k::NETWORK_SECTION_CONNECTION_TITLE),
            t(k::NETWORK_SECTION_CONNECTION_DESC),
            card.into_any_element(),
        )
    }

    fn render_test(&self, cx: &mut Context<Self>) -> gpui::Div {
        let incomplete = self.host_input.read(cx).content().trim().is_empty()
            || self.port_input.read(cx).content().trim().is_empty();
        let test = cx.listener(|this: &mut Self, _: &(), _window, cx| this.test_connection(cx));
        let row = layout::action_row(
            "network-proxy-test",
            t(k::NETWORK_TEST_LABEL),
            t(k::NETWORK_TEST_DESC),
            t(k::NETWORK_TEST_ACTION),
            ButtonTone::Neutral,
            self.saving || self.testing || incomplete,
            move |window, cx| test(&(), window, cx),
        );
        section(
            t(k::NETWORK_SECTION_TEST_TITLE),
            t(k::NETWORK_SECTION_TEST_DESC),
            layout::group(vec![row.into_any_element()]).into_any_element(),
        )
    }
}

impl Render for NetworkView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dirty = self.dirty(cx);
        let saving = self.saving;
        let discard = cx.listener(|this: &mut Self, _: &(), _window, cx| this.discard(cx));
        let save = cx.listener(|this: &mut Self, _: &(), _window, cx| this.save(cx));

        let column = layout::content_column()
            .child(self.render_connection(cx))
            .child(self.render_test(cx));

        layout::page()
            .relative()
            .child(layout::page_header(
                t(k::NETWORK_PAGE_TITLE),
                Some(t(k::NETWORK_PAGE_DESC)),
            ))
            .child(layout::scroll_body(
                "network-proxy-body",
                &self.scroll,
                column,
            ))
            .child(components::commit_bar(
                "network-proxy",
                dirty,
                saving,
                move |window, cx| discard(&(), window, cx),
                move |window, cx| save(&(), window, cx),
            ))
    }
}

crate::notifications::impl_status_toasts_leveled!(NetworkView);

fn section(title: SharedString, description: SharedString, body: AnyElement) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .w_full()
        .child(layout::section_header(title, description))
        .child(body)
}

fn error_text(error: FieldError, required: Key, invalid: Key) -> SharedString {
    match error {
        FieldError::Required => t(required),
        FieldError::Invalid => t(invalid),
    }
}

fn port_to_string(port: u16) -> String {
    if port == 0 {
        String::new()
    } else {
        port.to_string()
    }
}

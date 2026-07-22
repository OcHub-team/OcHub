//! OAuth account center. This ports cc-switch's AuthCenterPanel into GPUI and
//! calls the transport-agnostic managed-auth service directly.

use std::process::Command;
use std::sync::Arc;

use gpui::{div, prelude::*, Context, FontWeight, SharedString, Window};
use ochub_core::services::auth;
use ochub_core::{AppState, ManagedAuthAccount, ManagedAuthDeviceCodeResponse, ManagedAuthStatus};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::layout;
use crate::theme;

#[derive(Clone)]
struct LoginFlow {
    provider: &'static str,
    user_code: String,
    device_code: String,
    verification_uri: String,
}

/// 破坏性操作确认目标（移除账号 / 退出全部），携带展示所需信息。
#[derive(Clone)]
enum ConfirmAction {
    RemoveAccount {
        provider: &'static str,
        account_id: String,
        login: String,
    },
    Logout {
        provider: &'static str,
        provider_title: &'static str,
    },
}

pub struct AuthView {
    app: Arc<AppState>,
    copilot: Option<ManagedAuthStatus>,
    codex: Option<ManagedAuthStatus>,
    login: Option<LoginFlow>,
    /// 待确认的破坏性操作；`Some` 时展示确认模态。
    confirm: Option<ConfirmAction>,
    status: Option<SharedString>,
    busy: bool,
}

impl AuthView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            app,
            copilot: None,
            codex: None,
            login: None,
            confirm: None,
            status: None,
            busy: false,
        };
        this.reload(cx);
        this
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let copilot = auth::auth_get_status(&app, "github_copilot").await;
            let codex = auth::auth_get_status(&app, "codex_oauth").await;
            this.update(cx, |this, cx| {
                match copilot {
                    Ok(status) => this.copilot = Some(status),
                    Err(err) => {
                        this.status =
                            Some(SharedString::from(format!("读取 Copilot 认证失败: {err}")))
                    }
                }
                match codex {
                    Ok(status) => this.codex = Some(status),
                    Err(err) => {
                        this.status = Some(SharedString::from(format!(
                            "读取 Codex OAuth 认证失败: {err}"
                        )))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_login(&mut self, provider: &'static str, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(SharedString::from("正在获取设备码..."));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = auth::auth_start_login(&app, provider, None).await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(code) => {
                        this.login = Some(login_flow(provider, code));
                        this.status = Some(SharedString::from(
                            "设备码已生成，请打开验证页并输入用户码。",
                        ));
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("启动登录失败: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn poll_login(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.login.clone() else {
            self.status = Some(SharedString::from("没有正在进行的登录流程"));
            cx.notify();
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(SharedString::from("正在检查授权结果..."));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result =
                auth::auth_poll_for_account(&app, flow.provider, &flow.device_code, None).await;
            let copilot = auth::auth_get_status(&app, "github_copilot").await;
            let codex = auth::auth_get_status(&app, "codex_oauth").await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(Some(account)) => {
                        this.login = None;
                        this.status =
                            Some(SharedString::from(format!("已登录账号 {}", account.login)));
                    }
                    Ok(None) => {
                        this.status = Some(SharedString::from("仍在等待浏览器授权"));
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("轮询授权失败: {err}")));
                    }
                }
                if let Ok(status) = copilot {
                    this.copilot = Some(status);
                }
                if let Ok(status) = codex {
                    this.codex = Some(status);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_login_url(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.login.as_ref() else {
            self.status = Some(SharedString::from("没有可打开的验证页"));
            cx.notify();
            return;
        };
        match Command::new("open").arg(&flow.verification_uri).status() {
            Ok(status) if status.success() => {
                self.status = Some(SharedString::from("已打开验证页"));
            }
            Ok(status) => {
                self.status = Some(SharedString::from(format!("打开验证页失败: {status}")));
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("打开验证页失败: {err}")));
            }
        }
        cx.notify();
    }

    fn set_default(&mut self, provider: &'static str, account_id: String, cx: &mut Context<Self>) {
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = auth::auth_set_default_account(&app, provider, &account_id).await;
            let status = auth::auth_get_status(&app, provider).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.status = Some(SharedString::from("默认账号已更新")),
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("设置默认账号失败: {err}")))
                    }
                }
                if let Ok(status) = status {
                    this.assign_status(provider, status);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn remove_account(
        &mut self,
        provider: &'static str,
        account_id: String,
        cx: &mut Context<Self>,
    ) {
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = auth::auth_remove_account(&app, provider, &account_id).await;
            let status = auth::auth_get_status(&app, provider).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.status = Some(SharedString::from("账号已移除")),
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("移除账号失败: {err}")))
                    }
                }
                if let Ok(status) = status {
                    this.assign_status(provider, status);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn logout(&mut self, provider: &'static str, cx: &mut Context<Self>) {
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = auth::auth_logout(&app, provider).await;
            let status = auth::auth_get_status(&app, provider).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.status = Some(SharedString::from("已退出所有账号")),
                    Err(err) => this.status = Some(SharedString::from(format!("退出失败: {err}"))),
                }
                if let Ok(status) = status {
                    this.assign_status(provider, status);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn assign_status(&mut self, provider: &str, status: ManagedAuthStatus) {
        match provider {
            "github_copilot" => self.copilot = Some(status),
            "codex_oauth" => self.codex = Some(status),
            _ => {}
        }
    }

    fn render_account_row(
        &self,
        provider: &'static str,
        account: &ManagedAuthAccount,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let default_id = account.id.clone();
        let confirm_target = ConfirmAction::RemoveAccount {
            provider,
            account_id: account.id.clone(),
            login: account.login.clone(),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .w_full()
            .px_4()
            .py_2()
            .rounded_md()
            .bg(theme::surface_hover())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(account.login.clone())),
                    )
                    .child(div().text_color(theme::muted()).text_xs().truncate().child(
                        SharedString::from(format!("{} · {}", account.github_domain, account.id)),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .when(account.is_default, |s| {
                        s.child(components::badge(BadgeTone::Success, "默认"))
                    })
                    .when(!account.is_default, |s| {
                        s.child(
                            components::button(
                                format!("auth-default-{}-{}", provider, default_id),
                                "设为默认",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.set_default(provider, default_id.clone(), cx);
                                },
                            )),
                        )
                    })
                    .child(
                        components::button(
                            format!("auth-remove-{}-{}", provider, account.id),
                            "移除",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm = Some(confirm_target.clone());
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    fn render_provider_card(
        &self,
        provider: &'static str,
        title: &'static str,
        description: &'static str,
        status: Option<&ManagedAuthStatus>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let accounts = status
            .map(|s| s.accounts.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|account| self.render_account_row(provider, account, cx))
            .collect::<Vec<_>>();
        let authenticated = status.map(|s| s.authenticated).unwrap_or(false);
        let count = status.map(|s| s.accounts.len()).unwrap_or(0);
        let confirm_logout = ConfirmAction::Logout {
            provider,
            provider_title: title,
        };
        components::card()
            .gap_3()
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
                            .gap_1()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child(description),
                            ),
                    )
                    .child(if authenticated {
                        components::badge(BadgeTone::Success, format!("{count} 个账号"))
                    } else {
                        components::badge(BadgeTone::Neutral, "未认证")
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        components::button(
                            format!("auth-start-{provider}"),
                            "开始登录",
                            ButtonTone::Primary,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.start_login(provider, cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            format!("auth-logout-{provider}"),
                            "退出全部",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm = Some(confirm_logout.clone());
                                cx.notify();
                            },
                        )),
                    ),
            )
            .when(accounts.is_empty(), |s| {
                s.child(
                    div()
                        .text_color(theme::muted())
                        .text_xs()
                        .child("暂无账号。点击“开始登录”生成设备码。"),
                )
            })
            .children(accounts)
    }
}

impl Render for AuthView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let login = self.login.clone();
        layout::page()
            .relative()
            .child(
                layout::page_header(
                    "认证中心",
                    Some("管理 GitHub Copilot 和 ChatGPT OAuth 账号。".into()),
                )
                .child(
                    components::button("auth-refresh", "刷新", ButtonTone::Neutral, ButtonSize::Sm)
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.reload(cx);
                        })),
                ),
            )
            .child(components::status_footer(self.status.clone()))
            .child(layout::scroll_body(
                "auth-body",
                layout::content_column()
                    .gap_4()
                    .when_some(login, |s, flow| {
                        s.child(
                            components::card()
                                .gap_3()
                                .border_color(theme::yellow())
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_color(theme::text())
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child("等待浏览器授权"),
                                        )
                                        .child(components::badge(BadgeTone::Warning, "待操作")),
                                )
                                .child(div().text_color(theme::muted()).text_xs().child(
                                    SharedString::from(format!(
                                        "用户码：{} · 验证页：{}",
                                        flow.user_code, flow.verification_uri
                                    )),
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_2()
                                        .child(
                                            components::button(
                                                "auth-open-url",
                                                "打开验证页",
                                                ButtonTone::Primary,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.open_login_url(cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            components::button(
                                                "auth-poll",
                                                "我已授权",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.poll_login(cx);
                                                }),
                                            ),
                                        ),
                                ),
                        )
                    })
                    .child(self.render_provider_card(
                        "github_copilot",
                        "GitHub Copilot",
                        "管理 GitHub Copilot 账号，用于 Copilot 供应商和配额查询。",
                        self.copilot.as_ref(),
                        cx,
                    ))
                    .child(self.render_provider_card(
                        "codex_oauth",
                        "ChatGPT / Codex OAuth",
                        "管理 ChatGPT OAuth 账号，用于 Codex 官方 OAuth 供应商。",
                        self.codex.as_ref(),
                        cx,
                    )),
            ))
            .when_some(self.confirm.clone(), |root, action| {
                let (title, message, confirm_label) = match &action {
                    ConfirmAction::RemoveAccount { login, .. } => (
                        "移除账号",
                        format!("确定移除账号「{login}」吗？此操作不可撤销。"),
                        "移除",
                    ),
                    ConfirmAction::Logout { provider_title, .. } => (
                        "退出全部账号",
                        format!(
                            "确定退出 {provider_title} 的全部账号吗？需要重新登录后才能继续使用。"
                        ),
                        "退出全部",
                    ),
                };
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(title))
                        .child(
                            components::modal_body().child(
                                div()
                                    .text_color(theme::subtext())
                                    .text_sm()
                                    .child(SharedString::from(message)),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "auth-confirm-cancel",
                                "取消",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "auth-confirm-ok",
                                confirm_label,
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm = None;
                                match &action {
                                    ConfirmAction::RemoveAccount {
                                        provider,
                                        account_id,
                                        ..
                                    } => this.remove_account(*provider, account_id.clone(), cx),
                                    ConfirmAction::Logout { provider, .. } => {
                                        this.logout(*provider, cx)
                                    }
                                }
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
    }
}

fn login_flow(provider: &'static str, code: ManagedAuthDeviceCodeResponse) -> LoginFlow {
    LoginFlow {
        provider,
        user_code: code.user_code,
        device_code: code.device_code,
        verification_uri: code.verification_uri,
    }
}

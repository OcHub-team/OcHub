//! OAuth account center. This ports cc-switch's AuthCenterPanel into GPUI and
//! calls the transport-agnostic managed-auth service directly.

use std::process::Command;
use std::sync::Arc;

use gpui::{div, prelude::*, Context, FontWeight, SharedString, Window};
use ochub_core::services::auth;
use ochub_core::{AppState, ManagedAuthAccount, ManagedAuthDeviceCodeResponse, ManagedAuthStatus};

use crate::components;
use crate::layout;
use crate::theme;

#[derive(Clone)]
struct LoginFlow {
    provider: &'static str,
    user_code: String,
    device_code: String,
    verification_uri: String,
}

pub struct AuthView {
    app: Arc<AppState>,
    copilot: Option<ManagedAuthStatus>,
    codex: Option<ManagedAuthStatus>,
    login: Option<LoginFlow>,
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

    fn action_button(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        components::action_button(id, label, primary)
    }

    fn render_account_row(
        &self,
        provider: &'static str,
        account: &ManagedAuthAccount,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let default_id = account.id.clone();
        let remove_id = account.id.clone();
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
                    .gap_2()
                    .flex_shrink_0()
                    .when(account.is_default, |s| {
                        s.child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(theme::green())
                                .text_color(theme::accent_text())
                                .text_xs()
                                .child("默认"),
                        )
                    })
                    .when(!account.is_default, |s| {
                        s.child(
                            Self::action_button(
                                format!("auth-default-{}-{}", provider, default_id),
                                "设为默认",
                                false,
                            )
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.set_default(provider, default_id.clone(), cx);
                                },
                            )),
                        )
                    })
                    .child(
                        Self::action_button(
                            format!("auth-remove-{}-{}", provider, remove_id),
                            "移除",
                            false,
                        )
                        .text_color(theme::red())
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.remove_account(provider, remove_id.clone(), cx);
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
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
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
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(if authenticated {
                                theme::green()
                            } else {
                                theme::surface_hover()
                            })
                            .text_color(if authenticated {
                                theme::accent_text()
                            } else {
                                theme::subtext()
                            })
                            .text_xs()
                            .child(SharedString::from(if authenticated {
                                format!("{count} 个账号")
                            } else {
                                "未认证".to_string()
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        Self::action_button(format!("auth-start-{provider}"), "开始登录", true)
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.start_login(provider, cx);
                            })),
                    )
                    .child(
                        Self::action_button(format!("auth-logout-{provider}"), "退出全部", false)
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.logout(provider, cx);
                            })),
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
            .child(
                layout::page_header(
                    "认证中心",
                    Some("管理 GitHub Copilot 和 ChatGPT OAuth 账号。".into()),
                )
                .child(
                    Self::action_button("auth-refresh", "刷新", false).on_click(cx.listener(
                        |this, _event, _window, cx| {
                            this.reload(cx);
                        },
                    )),
                ),
            )
            .when_some(self.status.clone(), |s, status| {
                s.child(
                    div()
                        .px_6()
                        .py_2()
                        .text_color(theme::teal())
                        .text_xs()
                        .child(status),
                )
            })
            .child(layout::scroll_body(
                "auth-body",
                layout::content_column()
                    .gap_4()
                    .when_some(login, |s, flow| {
                        s.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .p_4()
                                .rounded_lg()
                                .bg(theme::surface())
                                .border_1()
                                .border_color(theme::yellow())
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("等待浏览器授权"),
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
                                            Self::action_button(
                                                "auth-open-url",
                                                "打开验证页",
                                                true,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.open_login_url(cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            Self::action_button("auth-poll", "我已授权", false)
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.poll_login(cx);
                                                    },
                                                )),
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

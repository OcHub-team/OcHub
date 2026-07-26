//! The About page: what this build is, and how it gets replaced.
//!
//! Split out of [`crate::settings_view`] rather than living on as one more
//! group on the settings root. The update controls own real machinery — a
//! download with progress, a signature check, an arm-and-restart that quits
//! the process — and none of it shares state with any other preference. Giving
//! it its own section keeps the settings page a list of preferences, and puts
//! the one screen that can replace the running binary where a user looks for
//! it.
//!
//! The i18n keys stay under `settings.about.*` / `settings.update.*`. They are
//! about this page's *content*, which did not change; renaming them would be a
//! mechanical churn across three locale catalogs for no behavioural gain.

use std::process::Command;

use gpui::{div, img, prelude::*, px, AnyElement, Context, ScrollHandle, SharedString, Window};
use ochub_core::services::UpdateCheckResult;
use ochub_core::settings::{self, AppSettings};

use crate::components::ButtonTone;
use crate::i18n::{k, raw, t};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::tf;
use crate::theme;

/// What the 检查更新 row knows. `info` is `None` until a check has run, which
/// is why the row reads "当前 x.y.z" rather than claiming to be up to date.
#[derive(Default)]
struct UpdateState {
    checking: bool,
    info: Option<UpdateCheckResult>,
    /// Set once the user starts an install. Blocks a second one and switches
    /// the row's description to download progress.
    installing: bool,
    /// Bytes downloaded and, when the server sent a length, the total.
    progress: Option<(u64, Option<u64>)>,
}

impl UpdateState {
    /// Whether the row's button should install rather than check.
    fn can_install(&self) -> bool {
        self.info
            .as_ref()
            .is_some_and(|info| info.has_update && info.can_self_install)
    }

    /// Download progress as a whole percentage, when the total is known.
    fn percent(&self) -> Option<u64> {
        match self.progress {
            Some((done, Some(total))) if total > 0 => Some(done.saturating_mul(100) / total),
            _ => None,
        }
    }
}

pub struct AboutView {
    /// Display cache, re-read after every write so the switch on screen always
    /// shows what is on disk.
    settings: AppSettings,
    update: UpdateState,
    /// Serializes the settings write and the release-page launch. Neither
    /// should be startable twice from one impatient double-click.
    busy: bool,
    scroll: ScrollHandle,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
}

impl AboutView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            settings: settings::get_settings(),
            update: UpdateState::default(),
            busy: false,
            scroll: ScrollHandle::new(),
            status: None,
            status_level: None,
        }
    }

    /// Re-read after an external write (the language switch repaints every
    /// view, and the auto-update flag is reachable from the settings search).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.settings = settings::get_settings();
        cx.notify();
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

    // ── Brand ───────────────────────────────────────────────────────────────

    /// The wordmark, in whichever polarity the current palette needs.
    ///
    /// Two files rather than one tinted asset: the mark is solid ink *plus* a
    /// fixed cyan accent, and a `text_color` tint the way [`crate::icons`]
    /// works would flatten that accent into the ink colour.
    ///
    /// The switch reads [`theme::is_dark`] rather than the window appearance
    /// because they disagree exactly where it matters: a user who pinned
    /// `ThemeMode::Light` under a dark system gets a light panel, and only the
    /// installed palette knows that. `-dark` means dark ink for light
    /// backgrounds, matching `docs/` and the site header.
    fn render_wordmark() -> AnyElement {
        let source = if theme::is_dark() {
            "brand/wordmark-light.png"
        } else {
            "brand/wordmark-dark.png"
        };
        // The sources are 480x119. Both dimensions are pinned to that ratio so
        // the mark cannot stretch if either asset is ever regenerated at a
        // different size.
        div()
            .w_full()
            .child(img(source).h(px(34.)).w(px(137.)))
            .into_any_element()
    }

    // ── Rows ────────────────────────────────────────────────────────────────

    fn render_auto_update_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let toggle = cx.listener(|this: &mut Self, _event: &(), _window: &mut Window, cx| {
            let target = !this.settings.auto_update_check;
            this.write(move |settings| settings.auto_update_check = target, cx);
        });
        layout::switch_row(
            "about-auto-update",
            t(k::SETTINGS_ABOUT_AUTOUPDATE_LABEL),
            t(k::SETTINGS_ABOUT_AUTOUPDATE_DESC),
            self.settings.auto_update_check,
            false,
            move |window, cx| toggle(&(), window, cx),
        )
        .into_any_element()
    }

    /// One button, two jobs: check until something is found, then install it.
    /// Matches the reference cc-switch behaviour, where the same control
    /// changes meaning once an update is known.
    fn render_update_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let installable = self.update.can_install();
        let activate = cx.listener(
            move |this: &mut Self, _event: &(), _window: &mut Window, cx| {
                if installable {
                    this.install_update(cx)
                } else {
                    this.check_updates(cx)
                }
            },
        );
        layout::action_row(
            "about-update",
            t(k::SETTINGS_ABOUT_UPDATE_LABEL),
            self.update_row_description(),
            if installable {
                t(k::SETTINGS_UPDATE_INSTALL)
            } else {
                t(k::SETTINGS_ABOUT_UPDATE_ACTION)
            },
            if installable {
                ButtonTone::Primary
            } else {
                ButtonTone::Neutral
            },
            self.update.checking || self.update.installing,
            move |window, cx| activate(&(), window, cx),
        )
        .into_any_element()
    }

    fn render_release_row(&self, cx: &mut Context<Self>) -> AnyElement {
        // Primary only once there is something to go and get.
        let tone = if self
            .update
            .info
            .as_ref()
            .is_some_and(|info| info.has_update)
        {
            ButtonTone::Primary
        } else {
            ButtonTone::Neutral
        };
        let activate = cx.listener(|this: &mut Self, _event: &(), _window: &mut Window, cx| {
            this.open_release_page(cx)
        });
        layout::action_row(
            "about-release",
            t(k::SETTINGS_ABOUT_RELEASE_LABEL),
            t(k::SETTINGS_ABOUT_RELEASE_DESC),
            t(k::SETTINGS_ACTION_OPEN),
            tone,
            false,
            move |window, cx| activate(&(), window, cx),
        )
        .into_any_element()
    }

    // ── Settings write ──────────────────────────────────────────────────────

    /// One targeted read-modify-write off the UI thread, followed by an
    /// authoritative re-read.
    fn write(
        &mut self,
        mutator: impl FnOnce(&mut AppSettings) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    settings::mutate_settings(mutator).map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => this.reload(cx),
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

    // ── Updates ─────────────────────────────────────────────────────────────

    fn update_row_description(&self) -> SharedString {
        if self.update.installing {
            return match self.update.percent() {
                // The last progress callback fires before the signature check,
                // so a stuck "100%" would look like a hang. Name what is
                // actually happening instead.
                Some(100) => t(k::SETTINGS_UPDATE_VERIFYING),
                Some(percent) => SharedString::from(tf!(
                    k::SETTINGS_UPDATE_DOWNLOADING,
                    percent = percent.to_string()
                )),
                // Nothing downloaded yet, or the server sent no content-length.
                None => t(k::SETTINGS_UPDATE_INSTALLING),
            };
        }
        if self.update.checking {
            return t(k::SETTINGS_UPDATE_CHECKING);
        }
        // An update exists but this install cannot apply it itself: say why
        // here rather than offering a button that opens a browser instead.
        if let Some(info) = &self.update.info {
            if info.has_update && !info.can_self_install {
                return SharedString::from(tf!(
                    k::SETTINGS_UPDATE_MANUAL_ONLY,
                    channel = info.install_channel.clone()
                ));
            }
        }
        match &self.update.info {
            Some(info) if info.has_update => SharedString::from(tf!(
                k::SETTINGS_ABOUT_UPDATE_UPGRADABLE,
                latest = info
                    .latest_version
                    .as_deref()
                    .unwrap_or(raw(k::SETTINGS_UPDATE_UNKNOWN_VERSION)),
                current = info.current_version
            )),
            Some(info) => SharedString::from(tf!(
                k::SETTINGS_ABOUT_UPDATE_UP_TO_DATE,
                version = info.current_version
            )),
            None => SharedString::from(tf!(
                k::SETTINGS_ABOUT_UPDATE_CURRENT,
                version = env!("CARGO_PKG_VERSION")
            )),
        }
    }

    fn check_updates(&mut self, cx: &mut Context<Self>) {
        if self.update.checking {
            return;
        }
        self.update.checking = true;
        self.set_status(NotificationLevel::Info, t(k::SETTINGS_UPDATE_CHECKING), cx);

        let task = cx.background_spawn(crate::core_async::run(
            ochub_core::services::update::check_for_updates(None),
        ));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.update.checking = false;
                match result {
                    Ok(info) => {
                        // An available update is a neutral finding; being up to
                        // date is the check completing with nothing to do.
                        let (level, message) = if info.has_update {
                            (
                                NotificationLevel::Info,
                                tf!(
                                    k::SETTINGS_UPDATE_AVAILABLE,
                                    latest = info
                                        .latest_version
                                        .as_deref()
                                        .unwrap_or(raw(k::SETTINGS_UPDATE_UNKNOWN_VERSION)),
                                    current = info.current_version
                                ),
                            )
                        } else {
                            (
                                NotificationLevel::Success,
                                tf!(
                                    k::SETTINGS_UPDATE_UP_TO_DATE,
                                    current = info.current_version
                                ),
                            )
                        };
                        this.set_status(level, message, cx);
                        this.update.info = Some(info);
                    }
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_UPDATE_CHECK_FAILED, error = err),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Download, verify, install, and restart.
    ///
    /// The download and signature check run on a background thread; only when
    /// they succeed does anything get replaced. Quitting is left to
    /// [`gpui::App::quit`] rather than `process::exit` so GPUI's own shutdown
    /// handlers still run — the window bounds saved in `main.rs` would
    /// otherwise be lost on every update, and the single-instance lock and
    /// gateway port would be released late enough to race the relaunch.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        if self.update.installing {
            return;
        }
        self.update.installing = true;
        self.update.progress = None;
        self.set_status(
            NotificationLevel::Info,
            t(k::SETTINGS_UPDATE_INSTALLING),
            cx,
        );

        // Progress arrives from the download thread; `report` hops back to the
        // UI thread to touch view state.
        let (progress_tx, mut progress_rx) =
            futures::channel::mpsc::unbounded::<(u64, Option<u64>)>();
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(update) = progress_rx.next().await {
                this.update(cx, |this, cx| {
                    this.update.progress = Some(update);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();

        let task = cx.background_spawn(crate::core_async::run(async move {
            let report = Box::new(move |done: u64, total: Option<u64>| {
                let _ = progress_tx.unbounded_send((done, total));
            });
            ochub_core::services::update::install::prepare(None, Some(report)).await
        }));

        cx.spawn(async move |this, cx| {
            let prepared = task.await;
            this.update(cx, |this, cx| {
                this.update.installing = false;
                this.update.progress = None;
                match prepared {
                    Ok(Some(prepared)) => {
                        let version = prepared.version.clone();
                        match ochub_core::services::update::apply_and_arm_restart(prepared) {
                            Ok(()) => {
                                this.set_status(
                                    NotificationLevel::Success,
                                    tf!(k::SETTINGS_UPDATE_INSTALLED, version = version),
                                    cx,
                                );
                                // The relaunch watcher is already waiting on
                                // this PID, so a clean quit is all that is
                                // left. Give the status line a moment first.
                                cx.spawn(async move |_this, cx| {
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(600))
                                        .await;
                                    cx.update(|cx| cx.quit());
                                })
                                .detach();
                            }
                            Err(err) => this.set_status(
                                NotificationLevel::Error,
                                tf!(k::SETTINGS_UPDATE_INSTALL_FAILED, error = err),
                                cx,
                            ),
                        }
                    }
                    // The manifest turned out not to be newer after all.
                    Ok(None) => this.set_status(
                        NotificationLevel::Success,
                        tf!(
                            k::SETTINGS_UPDATE_UP_TO_DATE,
                            current = ochub_core::services::update::current_version()
                        ),
                        cx,
                    ),
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_UPDATE_INSTALL_FAILED, error = err),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_release_page(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let url = self
            .update
            .info
            .as_ref()
            .map(|info| info.release_url.clone())
            .unwrap_or_else(|| ochub_core::services::latest_release_url(None));
        self.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { open_url(&url) }).await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => this.set_status(
                        NotificationLevel::Success,
                        t(k::SETTINGS_UPDATE_RELEASE_OPENED),
                        cx,
                    ),
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_UPDATE_RELEASE_OPEN_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Render for AboutView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = vec![
            self.render_auto_update_row(cx),
            self.render_update_row(cx),
            self.render_release_row(cx),
        ];
        layout::page()
            .child(layout::page_header(
                t(k::SETTINGS_ABOUT_TITLE),
                Some(t(k::SETTINGS_ABOUT_DESC)),
            ))
            .child(layout::scroll_body(
                "about-body",
                &self.scroll,
                layout::content_column()
                    .child(Self::render_wordmark())
                    .child(layout::group(rows)),
            ))
    }
}

crate::notifications::impl_status_toasts_leveled!(AboutView);

fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        cmd
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };

    cmd.status()
        .map_err(|err| err.to_string())
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(tf!(k::SETTINGS_ERROR_EXIT_STATUS, status = status))
            }
        })
}

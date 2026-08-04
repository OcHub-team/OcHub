//! Sessions panel. Lists recent CLI sessions discovered on disk via
//! `session_manager::scan_sessions()` and supports deleting one. Scanning and
//! transcript loading are filesystem-heavy, so both run on the background
//! executor; scan results are cached for [`SCAN_TTL`] so re-entering the
//! section doesn't rescan every time.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{
    Datelike, Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike,
};
use futures::StreamExt;
use gpui::{
    Anchor, AnyElement, Context, ElementId, Entity, FontWeight, ListAlignment, ListState,
    MouseButton, ScrollHandle, SharedString, Task, Window, anchored, deferred, div, point,
    prelude::*, px, relative,
};
use ochub_core::session_index::{SearchHit, SessionIndex};
use ochub_core::session_manager::{SessionMessage, SessionMeta};
use ochub_core::{AppId, AppState};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::icons::{IconName, icon};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::remote::WorkspaceBackend;
use crate::text_input::TextInput;
use crate::tf;
use crate::theme;

const DEFAULT_PAGE_SIZE: usize = 20;
const PAGE_SIZE_OPTIONS: &[u32] = &[20, 50, 100];

/// How long a completed scan stays fresh; re-entering the section within this
/// window shows the cached list instantly (刷新按钮无视 TTL 强制重扫).
const SCAN_TTL: Duration = Duration::from_secs(30);

/// Long tool outputs and pasted files can contain hundreds of thousands of
/// characters. Keep the default layout bounded; users can still expand any
/// individual message when they need the full text.
const MESSAGE_PREVIEW_CHARS: usize = 3_000;

/// How long typing has to pause before a full-text query runs. Long enough to
/// swallow a typing burst, short enough that the results feel keystroke-driven:
/// the search itself costs under a millisecond above three characters, and ~65 ms
/// at its worst below that.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Upper bound on sessions returned by one full-text query. Far above what any
/// useful search returns, and there only so a query like a single common
/// character cannot pull the entire index into memory.
const SEARCH_HIT_LIMIT: usize = 2_000;

/// Time budget for one index maintenance slice, kept short enough to stay
/// invisible even if it lands while the user is interacting with the list.
const MAINTENANCE_BUDGET: Duration = Duration::from_millis(400);

/// Smallest gap between two index progress updates. A sync walks every session
/// on the machine, and repainting the panel once per session would cost more
/// than the indexing itself; a counter is unreadable faster than this anyway.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);

/// Progress of the background job that brings the index up to date.
#[derive(Clone, PartialEq, Eq)]
enum IndexSyncState {
    Idle,
    Syncing {
        done: usize,
        total: usize,
        /// The index held nothing when this pass started, so search cannot
        /// answer anything until it finishes. Drives the waiting page.
        cold: bool,
    },
    Failed(SharedString),
}

/// What the background sync reports back.
///
/// [`Started`](SyncProgress::Started) arrives before any indexing work, because
/// how much the index already covers decides whether this pass is worth waiting
/// for or should stay a one-line hint above an otherwise usable list.
enum SyncProgress {
    Started { covered: usize },
    Advanced { done: usize },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionDateFilter {
    All,
    Today,
    SevenDays,
    ThirtyDays,
    Custom { start_ms: i64, end_ms: i64 },
}

impl SessionDateFilter {
    fn label(self) -> String {
        match self {
            Self::All => raw(k::SESSIONS_FILTER_DATE_ALL).to_string(),
            Self::Today => raw(k::SESSIONS_FILTER_DATE_TODAY).to_string(),
            Self::SevenDays => raw(k::SESSIONS_FILTER_DATE_SEVEN_DAYS).to_string(),
            Self::ThirtyDays => raw(k::SESSIONS_FILTER_DATE_THIRTY_DAYS).to_string(),
            Self::Custom { start_ms, end_ms } => {
                let start = Local
                    .timestamp_millis_opt(start_ms)
                    .single()
                    .map(|value| value.format("%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| raw(k::SESSIONS_FILTER_DATE_CUSTOM).to_string());
                let end = Local
                    .timestamp_millis_opt(end_ms)
                    .single()
                    .map(|value| value.format("%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                format!("{start} – {end}")
            }
        }
    }

    fn matches(self, timestamp_ms: Option<i64>) -> bool {
        if self == Self::All {
            return true;
        }
        let Some(timestamp_ms) = timestamp_ms else {
            return false;
        };
        let Some(active_time) = Local.timestamp_millis_opt(timestamp_ms).single() else {
            return false;
        };
        let active_date = active_time.date_naive();
        let today = Local::now().date_naive();
        match self {
            Self::All => true,
            Self::Today => active_date == today,
            Self::SevenDays => {
                active_date >= today - ChronoDuration::days(6) && active_date <= today
            }
            Self::ThirtyDays => {
                active_date >= today - ChronoDuration::days(29) && active_date <= today
            }
            Self::Custom { start_ms, end_ms } => timestamp_ms >= start_ms && timestamp_ms <= end_ms,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionFilterPopover {
    Date,
    App,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionRangeEndpoint {
    Start,
    End,
}

struct PreparedSessionMessage {
    role: String,
    content: SharedString,
    preview: SharedString,
    is_long: bool,
    ts: Option<i64>,
}

#[derive(Default)]
struct SessionStats {
    user_messages: usize,
    assistant_messages: usize,
    tool_messages: usize,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
}

/// An opened session: its metadata plus the loaded conversation transcript.
struct SessionDetail {
    meta: SessionMeta,
    messages: Vec<PreparedSessionMessage>,
    stats: SessionStats,
    error: Option<SharedString>,
    /// The message a search result pointed at, marked so the eye lands on it
    /// after the transcript scrolls there.
    focused_message: Option<usize>,
}

pub struct SessionsView {
    backend: WorkspaceBackend,
    workspace_available: bool,
    sessions: Vec<SessionMeta>,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
    /// Zero-based current page into `sessions`.
    page: usize,
    page_size: usize,
    page_size_open: bool,
    filtered_indices: Vec<usize>,
    visible_session_indices: Vec<usize>,
    app_options: Vec<String>,
    /// When `Some`, the transcript viewer replaces the list.
    detail: Option<SessionDetail>,
    /// Session index pending deletion confirmation; when `Some`, a modal is shown.
    confirm_delete: Option<usize>,
    /// A background scan is in flight (suppresses duplicate scans).
    scanning: bool,
    /// Session index whose transcript is currently loading.
    loading_detail: Option<usize>,
    /// When the last scan finished; drives the [`SCAN_TTL`] freshness check.
    last_scan: Option<Instant>,
    date_filter: SessionDateFilter,
    app_filter: Option<String>,
    open_filter_popover: Option<SessionFilterPopover>,
    date_filter_error: Option<SharedString>,
    active_datetime_picker: Option<SessionRangeEndpoint>,
    picker_year: i32,
    picker_month: u32,
    picker_hour_scroll: ScrollHandle,
    picker_minute_scroll: ScrollHandle,
    app_filter_scroll: ScrollHandle,
    empty_scroll: ScrollHandle,
    session_list_state: ListState,
    /// Drives the transcript's variable-height virtual list.
    transcript_list_state: ListState,
    /// Message indexes explicitly expanded by the user.
    expanded_messages: HashSet<usize>,
    page_input: Entity<TextInput>,
    range_start_input: Entity<TextInput>,
    range_end_input: Entity<TextInput>,
    search_input: Entity<TextInput>,
    /// Last content read off `search_input`, so cursor blinks and other
    /// unrelated notifications do not re-run the search.
    last_search_content: SharedString,
    /// The query the list is currently filtered by, lowercased once here rather
    /// than on every row of every frame.
    query: String,
    /// Transcript matches for [`Self::query`], keyed by session source path.
    /// Empty while the index is off — the title match still applies.
    content_hits: HashMap<String, SearchHit>,
    /// Bumped per keystroke so a query that resolves out of order is discarded.
    search_generation: u64,
    /// Holds the debounce timer; dropping it cancels the pending search.
    search_task: Option<Task<()>>,
    searching: bool,
    index_enabled: bool,
    index_state: IndexSyncState,
    /// Set once the user asks to browse past the first-build waiting page. The
    /// build carries on behind the list.
    index_wait_dismissed: bool,
    /// Set to cancel an in-flight index sync when the user leaves or switches
    /// the feature off.
    index_cancel: Arc<AtomicBool>,
    index_task: Option<Task<()>>,
    workspace_generation: u64,
}

impl SessionsView {
    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// `refresh_windows` re-runs `render`, but gpui's virtualized lists cache
    /// measured item heights and invalidate them only on a width change, so a
    /// translation that changes a row's height would otherwise leave the list
    /// scrolled to stale offsets.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        // The placeholder is captured when the input is constructed, and this
        // view is built once at startup, so it needs pushing in by hand.
        self.page_input.update(cx, |input, cx| {
            input.set_placeholder(t(k::SESSIONS_PAGINATION_PAGE_PLACEHOLDER), cx)
        });
        self.session_list_state.remeasure();
        self.transcript_list_state.remeasure();
        cx.notify();
    }

    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let now = Local::now();
        let this = Self {
            backend: WorkspaceBackend::local(app),
            workspace_available: true,
            sessions: Vec::new(),
            status: None,
            status_level: None,
            page: 0,
            page_size: DEFAULT_PAGE_SIZE,
            page_size_open: false,
            filtered_indices: Vec::new(),
            visible_session_indices: Vec::new(),
            app_options: Vec::new(),
            detail: None,
            confirm_delete: None,
            scanning: false,
            loading_detail: None,
            last_scan: None,
            date_filter: SessionDateFilter::All,
            app_filter: None,
            open_filter_popover: None,
            date_filter_error: None,
            active_datetime_picker: None,
            picker_year: now.year(),
            picker_month: now.month(),
            picker_hour_scroll: ScrollHandle::new(),
            picker_minute_scroll: ScrollHandle::new(),
            app_filter_scroll: ScrollHandle::new(),
            empty_scroll: ScrollHandle::new(),
            session_list_state: ListState::new(0, ListAlignment::Top, px(96.)),
            transcript_list_state: ListState::new(0, ListAlignment::Top, px(320.)),
            expanded_messages: HashSet::new(),
            page_input: cx
                .new(|cx| text_input(cx, t(k::SESSIONS_PAGINATION_PAGE_PLACEHOLDER)).compact()),
            range_start_input: cx.new(|cx| text_input(cx, "YYYY/MM/DD HH:mm:ss")),
            range_end_input: cx.new(|cx| text_input(cx, "YYYY/MM/DD HH:mm:ss")),
            search_input: cx.new(|cx| text_input(cx, t(k::SESSIONS_SEARCH_PLACEHOLDER))),
            last_search_content: SharedString::default(),
            query: String::new(),
            content_hits: HashMap::new(),
            search_generation: 0,
            search_task: None,
            searching: false,
            index_enabled: ochub_core::settings::get_settings().session_index_enabled,
            index_state: IndexSyncState::Idle,
            index_wait_dismissed: false,
            index_cancel: Arc::new(AtomicBool::new(false)),
            index_task: None,
            workspace_generation: 0,
        };
        // Do not scan here: AppRoot eagerly constructs every section. The
        // shell calls `reload` when Sessions is actually selected.
        // “跳至 X 页”回车提交。
        let jump = cx.listener(|this: &mut Self, _event: &(), _window, cx| {
            let text = this.page_input.read(cx).content().trim().to_string();
            if let Ok(target) = text.parse::<usize>()
                && target >= 1
            {
                let last = this.total_pages().saturating_sub(1);
                this.set_page((target - 1).min(last), cx);
            }
        });
        this.page_input.update(cx, |input, _| {
            input.set_on_enter(move |window, cx| jump(&(), window, cx));
        });
        let apply_start = cx.listener(|this: &mut Self, _event: &(), _window, cx| {
            this.apply_custom_range(cx);
        });
        this.range_start_input.update(cx, |input, _| {
            input.set_on_enter(move |window, cx| apply_start(&(), window, cx));
        });
        // Narrow on titles as each keystroke lands, and queue the full-text
        // query behind a debounce.
        cx.observe(&this.search_input, |this, input, cx| {
            let content = input.read(cx).content();
            if content != this.last_search_content {
                this.last_search_content = content.clone();
                this.set_query(content.trim().to_string(), cx);
            }
        })
        .detach();

        let apply_end = cx.listener(|this: &mut Self, _event: &(), _window, cx| {
            this.apply_custom_range(cx);
        });
        this.range_end_input.update(cx, |input, _| {
            input.set_on_enter(move |window, cx| apply_end(&(), window, cx));
        });
        this
    }

    /// Re-entering the section: close any open transcript and rescan in the
    /// background — unless the cached list is still fresh, in which case it
    /// shows instantly with no IO at all.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.detail = None;
        if !self.workspace_available {
            self.sessions.clear();
            self.rebuild_session_index();
            self.set_status(
                tf!(
                    k::SESSIONS_STATUS_LOAD_FAILED,
                    error = "remote workspace is not connected"
                ),
                NotificationLevel::Error,
            );
            cx.notify();
            return;
        }
        self.refresh_index_enabled(cx);
        let fresh = self.last_scan.is_some_and(|at| at.elapsed() < SCAN_TTL);
        if fresh || self.scanning {
            // The list is still fresh, but the index may not be: a session
            // touched since the last scan, or the feature only just switched on.
            self.start_index_sync(cx);
            cx.notify();
            return;
        }
        self.start_scan(cx);
    }

    pub(crate) fn set_workspace(&mut self, backend: WorkspaceBackend, cx: &mut Context<Self>) {
        self.workspace_generation = self.workspace_generation.wrapping_add(1);
        self.search_generation = self.search_generation.wrapping_add(1);
        self.backend = backend;
        self.workspace_available = true;
        self.scanning = false;
        self.loading_detail = None;
        self.searching = false;
        self.search_task = None;
        self.last_scan = None;
        self.sessions.clear();
        self.detail = None;
        self.confirm_delete = None;
        self.content_hits.clear();
        self.rebuild_session_index();
        self.reload(cx);
    }

    pub(crate) fn set_workspace_unavailable(&mut self, cx: &mut Context<Self>) {
        self.workspace_generation = self.workspace_generation.wrapping_add(1);
        self.search_generation = self.search_generation.wrapping_add(1);
        self.workspace_available = false;
        self.scanning = false;
        self.loading_detail = None;
        self.searching = false;
        self.search_task = None;
        self.last_scan = None;
        self.detail = None;
        self.confirm_delete = None;
        self.reload(cx);
    }

    /// The refresh button: always rescan, ignoring the TTL.
    fn force_reload(&mut self, cx: &mut Context<Self>) {
        if !self.workspace_available {
            return;
        }
        self.detail = None;
        if !self.scanning {
            self.start_scan(cx);
        }
    }

    fn start_scan(&mut self, cx: &mut Context<Self>) {
        if !self.workspace_available {
            return;
        }
        self.scanning = true;
        cx.notify();
        let backend = self.backend.clone();
        let generation = self.workspace_generation;
        cx.spawn(async move |this, cx| {
            let sessions =
                crate::core_async::run(async move { backend.list_sessions(None, None).await })
                    .await;
            this.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
                match sessions {
                    Ok(sessions) => this.sessions = sessions,
                    Err(error) => {
                        this.sessions.clear();
                        this.set_status(
                            tf!(k::SESSIONS_STATUS_LOAD_FAILED, error = error),
                            NotificationLevel::Error,
                        );
                    }
                }
                this.scanning = false;
                this.last_scan = Some(Instant::now());
                this.rebuild_session_index();
                this.start_index_sync(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn total_pages(&self) -> usize {
        self.filtered_indices.len().div_ceil(self.page_size).max(1)
    }

    fn session_matches_filters(&self, session: &SessionMeta) -> bool {
        let app_matches = self
            .app_filter
            .as_deref()
            .is_none_or(|app| session.provider_id == app);
        let timestamp = session.last_active_at.or(session.created_at);
        app_matches && self.date_filter.matches(timestamp) && self.session_matches_query(session)
    }

    /// A session matches the query if its metadata does, or if the index found
    /// the query in its transcript.
    ///
    /// The two are unioned rather than ranked: metadata matching costs nothing
    /// and works with the index switched off, so it stays the floor the search
    /// box always provides.
    fn session_matches_query(&self, session: &SessionMeta) -> bool {
        if self.query.is_empty() {
            return true;
        }
        if session
            .source_path
            .as_deref()
            .is_some_and(|path| self.content_hits.contains_key(path))
        {
            return true;
        }
        metadata_matches(session, &self.query)
    }

    fn content_hit_for(&self, session: &SessionMeta) -> Option<&SearchHit> {
        if self.query.is_empty() {
            return None;
        }
        self.content_hits.get(session.source_path.as_deref()?)
    }

    /// Apply a new query: narrow on metadata now, and schedule the full-text
    /// pass for when typing pauses.
    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        let query = query.to_lowercase();
        if query == self.query {
            return;
        }
        self.query = query;
        self.page = 0;
        // Drop the previous hits rather than keeping them until replacements
        // arrive: they answer a query the user has already moved on from.
        self.content_hits.clear();
        self.search_generation = self.search_generation.wrapping_add(1);
        self.rebuild_session_index();

        if self.query.is_empty() || !self.index_enabled || !self.workspace_available {
            self.searching = false;
            self.search_task = None;
            cx.notify();
            return;
        }

        self.searching = true;
        let generation = self.search_generation;
        let query = self.query.clone();
        let backend = self.backend.clone();
        let remote = self.backend.is_remote();
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let hits = if remote {
                crate::core_async::run(async move {
                    backend
                        .search_session_index(&query, SEARCH_HIT_LIMIT)
                        .await
                        .map_err(|error| error.to_string())
                })
                .await
            } else {
                cx.background_spawn(async move {
                    SessionIndex::open().and_then(|index| index.search(&query, SEARCH_HIT_LIMIT))
                })
                .await
            };
            this.update(cx, |this, cx| {
                // A later keystroke has already superseded this query.
                if this.search_generation != generation {
                    return;
                }
                this.searching = false;
                if let Ok(hits) = hits {
                    this.content_hits = hits
                        .into_iter()
                        .map(|hit| (hit.source_path.clone(), hit))
                        .collect();
                }
                this.rebuild_session_index();
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Bring the index in line with the sessions just scanned, then reclaim any
    /// space that has piled up.
    ///
    /// Runs after a scan because the scan already produced the session list the
    /// sync needs, and because that is the moment the user is demonstrably
    /// looking at this panel.
    fn start_index_sync(&mut self, cx: &mut Context<Self>) {
        if !self.workspace_available || !self.index_enabled || self.sessions.is_empty() {
            return;
        }
        if matches!(self.index_state, IndexSyncState::Syncing { .. }) {
            return;
        }
        if self.backend.is_remote() {
            let backend = self.backend.clone();
            let total = self.sessions.len();
            self.index_wait_dismissed = false;
            self.index_state = IndexSyncState::Syncing {
                done: 0,
                total,
                cold: self.content_hits.is_empty(),
            };
            self.index_task = Some(cx.spawn(async move |this, cx| {
                let result =
                    crate::core_async::run(async move { backend.build_session_index().await })
                        .await;
                this.update(cx, |this, cx| {
                    this.index_state = match result {
                        Ok(_) => IndexSyncState::Idle,
                        Err(error) => IndexSyncState::Failed(SharedString::from(error.to_string())),
                    };
                    if !this.query.is_empty() {
                        let query = std::mem::take(&mut this.query);
                        this.set_query(query, cx);
                    }
                    cx.notify();
                })
                .ok();
            }));
            cx.notify();
            return;
        }

        self.index_cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.index_cancel.clone();
        let sessions = self.sessions.clone();
        let auto_reclaim = ochub_core::settings::get_settings().session_index_auto_reclaim;
        self.index_wait_dismissed = false;
        self.index_state = IndexSyncState::Syncing {
            done: 0,
            total: sessions.len(),
            cold: false,
        };
        cx.notify();

        self.index_task = Some(cx.spawn(async move |this, cx| {
            // An *async* channel, because this task runs on the foreground
            // executor — `cx.spawn` schedules onto the main thread. A
            // `std::sync::mpsc` receiver would park that thread rather than
            // yield, and since the loop below has no other await point the whole
            // sync would run inside one poll: the window would freeze until the
            // last session was indexed, progress included.
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<SyncProgress>();
            let worker = cx.background_spawn(async move {
                let index = SessionIndex::open()?;
                let covered = index
                    .stats()
                    .map(|stats| stats.sessions.max(0) as usize)
                    .unwrap_or(0);
                let _ = tx.unbounded_send(SyncProgress::Started { covered });

                let mut last_sent: Option<Instant> = None;
                let outcome = index.sync(
                    &sessions,
                    |done, total| {
                        // Throttled: the callback fires once per session, and a
                        // repaint each time would cost more than the indexing.
                        // The final tick always goes through so the counter
                        // lands on its total rather than stopping just short.
                        let due = last_sent.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL);
                        if due || done == total {
                            last_sent = Some(Instant::now());
                            let _ = tx.unbounded_send(SyncProgress::Advanced { done });
                        }
                    },
                    &cancel,
                )?;
                if auto_reclaim && !outcome.cancelled && index.needs_maintenance()? {
                    index.maintain(MAINTENANCE_BUDGET)?;
                }
                Ok::<_, String>(outcome)
            });

            // Forward progress on the foreground so the panel can show it. Each
            // `next().await` hands the main thread back to the run loop, so
            // frames keep being drawn; the loop ends when the worker drops its
            // sender.
            while let Some(progress) = rx.next().await {
                let updated = this
                    .update(cx, |this, cx| {
                        let IndexSyncState::Syncing { done, cold, .. } = &mut this.index_state
                        else {
                            return;
                        };
                        match progress {
                            // Nothing indexed yet: search cannot answer anything
                            // until this pass lands, which is what makes it worth
                            // waiting on.
                            SyncProgress::Started { covered } => *cold = covered == 0,
                            SyncProgress::Advanced { done: reached } => *done = reached,
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !updated {
                    break;
                }
            }

            let result = worker.await;
            this.update(cx, |this, cx| {
                this.index_state = match result {
                    Ok(_) => IndexSyncState::Idle,
                    Err(error) => IndexSyncState::Failed(SharedString::from(error)),
                };
                // A query typed while the index was still filling was answered
                // against a partial index; re-run it now that it is complete.
                if !this.query.is_empty() {
                    let query = std::mem::take(&mut this.query);
                    this.set_query(query, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn index_wait_progress(&self) -> Option<(usize, usize)> {
        index_wait_progress(&self.index_state, self.index_wait_dismissed)
    }

    /// The first-build waiting page.
    ///
    /// The wait is deliberately not mandatory: the session list itself does not
    /// depend on the index, so holding it back would be a cost with nothing
    /// bought. The escape hatch dismisses the page and leaves the build running.
    fn render_index_wait(&self, done: usize, total: usize, cx: &mut Context<Self>) -> AnyElement {
        let fraction = if total == 0 {
            0.
        } else {
            (done as f32 / total as f32).clamp(0., 1.)
        };

        layout::scroll_body(
            "session-index-wait-body",
            &self.empty_scroll,
            layout::content_column().child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .w_full()
                    .gap_2()
                    .py_12()
                    .child(icon(IconName::Refresh, theme::muted(), 26.))
                    .child(
                        div()
                            .text_color(theme::subtext())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t(k::SESSIONS_INDEX_WAIT_TITLE)),
                    )
                    .child(
                        div()
                            .max_w(px(420.))
                            .text_color(theme::muted())
                            .text_xs()
                            .child(t(k::SESSIONS_INDEX_WAIT_HINT)),
                    )
                    .child(
                        div()
                            .mt_2()
                            .w_full()
                            .max_w(px(320.))
                            .h(px(6.))
                            .rounded_full()
                            .bg(theme::inset())
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(fraction))
                                    .rounded_full()
                                    .bg(theme::accent()),
                            ),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(SharedString::from(tf!(
                                k::SESSIONS_SEARCH_INDEXING,
                                done = done.to_string(),
                                total = total.to_string()
                            ))),
                    )
                    .child(
                        div().mt_2().child(
                            components::button(
                                "sessions-index-wait-skip",
                                t(k::SESSIONS_INDEX_WAIT_SKIP),
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.index_wait_dismissed = true;
                                    cx.notify();
                                },
                            )),
                        ),
                    ),
            ),
        )
        .into_any_element()
    }

    /// Pick up an index setting changed elsewhere (the settings page), without
    /// making the two views talk to each other directly.
    fn refresh_index_enabled(&mut self, cx: &mut Context<Self>) {
        if !self.workspace_available {
            return;
        }
        if self.backend.is_remote() {
            let backend = self.backend.clone();
            cx.spawn(async move |this, cx| {
                let enabled = crate::core_async::run(async move {
                    backend
                        .setting("sessionIndexEnabled")
                        .await
                        .map(|value| value.as_bool().unwrap_or(false))
                })
                .await
                .unwrap_or(false);
                this.update(cx, |this, cx| {
                    this.apply_index_enabled(enabled, cx);
                    if enabled {
                        this.start_index_sync(cx);
                    }
                })
                .ok();
            })
            .detach();
            return;
        }
        let enabled = ochub_core::settings::get_settings().session_index_enabled;
        self.apply_index_enabled(enabled, cx);
    }

    fn apply_index_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if enabled == self.index_enabled {
            return;
        }
        self.index_enabled = enabled;
        if !enabled {
            self.index_cancel.store(true, Ordering::Relaxed);
            self.index_task = None;
            self.index_state = IndexSyncState::Idle;
            self.content_hits.clear();
            self.rebuild_session_index();
        }
        cx.notify();
    }

    fn filtered_session_count(&self) -> usize {
        self.filtered_indices.len()
    }

    fn rebuild_session_index(&mut self) {
        self.app_options = self
            .sessions
            .iter()
            .map(|session| session.provider_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.filtered_indices = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| self.session_matches_filters(session).then_some(index))
            .collect();
        let max_page = self.total_pages().saturating_sub(1);
        self.page = self.page.min(max_page);
        self.rebuild_visible_sessions();
    }

    fn rebuild_visible_sessions(&mut self) {
        let start = self.page.saturating_mul(self.page_size);
        let end = (start + self.page_size).min(self.filtered_indices.len());
        self.visible_session_indices = if start < end {
            self.filtered_indices[start..end].to_vec()
        } else {
            Vec::new()
        };
        self.session_list_state
            .reset(self.visible_session_indices.len());
    }

    fn set_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let max_page = self.total_pages().saturating_sub(1);
        let page = page.min(max_page);
        if page != self.page {
            self.page = page;
            self.page_size_open = false;
            self.rebuild_visible_sessions();
            cx.notify();
        }
    }

    fn toggle_page_size(&mut self, cx: &mut Context<Self>) {
        self.page_size_open = !self.page_size_open;
        cx.notify();
    }

    fn set_page_size(&mut self, page_size: usize, cx: &mut Context<Self>) {
        if self.page_size != page_size {
            self.page_size = page_size;
            self.page = 0;
            self.rebuild_visible_sessions();
        }
        self.page_size_open = false;
        cx.notify();
    }

    fn set_date_filter(&mut self, filter: SessionDateFilter, cx: &mut Context<Self>) {
        self.date_filter = filter;
        if !matches!(filter, SessionDateFilter::Custom { .. }) {
            self.range_start_input
                .update(cx, |input, cx| input.set_content("", cx));
            self.range_end_input
                .update(cx, |input, cx| input.set_content("", cx));
        }
        self.page = 0;
        self.open_filter_popover = None;
        self.active_datetime_picker = None;
        self.date_filter_error = None;
        self.rebuild_session_index();
        cx.notify();
    }

    fn set_app_filter(&mut self, app: Option<String>, cx: &mut Context<Self>) {
        self.app_filter = app;
        self.page = 0;
        self.open_filter_popover = None;
        self.rebuild_session_index();
        cx.notify();
    }

    fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.date_filter = SessionDateFilter::All;
        self.app_filter = None;
        self.search_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.last_search_content = SharedString::default();
        self.query.clear();
        self.content_hits.clear();
        self.searching = false;
        self.search_task = None;
        self.page = 0;
        self.open_filter_popover = None;
        self.active_datetime_picker = None;
        self.date_filter_error = None;
        self.range_start_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.range_end_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.rebuild_session_index();
        cx.notify();
    }

    fn toggle_filter_popover(&mut self, popover: SessionFilterPopover, cx: &mut Context<Self>) {
        self.open_filter_popover = if self.open_filter_popover == Some(popover) {
            None
        } else {
            Some(popover)
        };
        if popover != SessionFilterPopover::Date {
            self.active_datetime_picker = None;
        }
        self.date_filter_error = None;
        cx.notify();
    }

    fn endpoint_input(&self, endpoint: SessionRangeEndpoint) -> &Entity<TextInput> {
        match endpoint {
            SessionRangeEndpoint::Start => &self.range_start_input,
            SessionRangeEndpoint::End => &self.range_end_input,
        }
    }

    fn endpoint_datetime(
        &self,
        endpoint: SessionRangeEndpoint,
        cx: &mut Context<Self>,
    ) -> chrono::DateTime<Local> {
        let value = self
            .endpoint_input(endpoint)
            .read(cx)
            .content()
            .trim()
            .to_string();
        if let Some(value) = parse_local_datetime(&value, endpoint == SessionRangeEndpoint::End) {
            return value;
        }
        if let SessionDateFilter::Custom { start_ms, end_ms } = self.date_filter {
            let timestamp = match endpoint {
                SessionRangeEndpoint::Start => start_ms,
                SessionRangeEndpoint::End => end_ms,
            };
            if let Some(value) = Local.timestamp_millis_opt(timestamp).single() {
                return value;
            }
        }
        Local::now()
    }

    fn toggle_datetime_picker(&mut self, endpoint: SessionRangeEndpoint, cx: &mut Context<Self>) {
        if self.active_datetime_picker == Some(endpoint) {
            self.active_datetime_picker = None;
        } else {
            let selected = self.endpoint_datetime(endpoint, cx);
            self.picker_year = selected.year();
            self.picker_month = selected.month();
            self.picker_hour_scroll
                .scroll_to_top_of_item(selected.hour() as usize);
            self.picker_minute_scroll
                .scroll_to_top_of_item(selected.minute() as usize);
            self.active_datetime_picker = Some(endpoint);
        }
        cx.notify();
    }

    fn update_datetime_endpoint(
        &mut self,
        endpoint: SessionRangeEndpoint,
        date: NaiveDate,
        hour: u32,
        minute: u32,
        cx: &mut Context<Self>,
    ) {
        let second = if endpoint == SessionRangeEndpoint::End {
            59
        } else {
            0
        };
        let Some(value) = Local
            .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, second)
            .earliest()
        else {
            return;
        };
        let input = self.endpoint_input(endpoint).clone();
        input.update(cx, |input, cx| {
            input.set_content(value.format("%Y/%m/%d %H:%M:%S").to_string(), cx)
        });
        self.date_filter_error = None;
        cx.notify();
    }

    fn select_picker_date(
        &mut self,
        endpoint: SessionRangeEndpoint,
        date: NaiveDate,
        cx: &mut Context<Self>,
    ) {
        let current = self.endpoint_datetime(endpoint, cx);
        self.picker_year = date.year();
        self.picker_month = date.month();
        self.update_datetime_endpoint(endpoint, date, current.hour(), current.minute(), cx);
    }

    fn select_picker_hour(
        &mut self,
        endpoint: SessionRangeEndpoint,
        hour: u32,
        cx: &mut Context<Self>,
    ) {
        let current = self.endpoint_datetime(endpoint, cx);
        self.update_datetime_endpoint(endpoint, current.date_naive(), hour, current.minute(), cx);
        self.picker_hour_scroll.scroll_to_top_of_item(hour as usize);
    }

    fn select_picker_minute(
        &mut self,
        endpoint: SessionRangeEndpoint,
        minute: u32,
        cx: &mut Context<Self>,
    ) {
        let current = self.endpoint_datetime(endpoint, cx);
        self.update_datetime_endpoint(endpoint, current.date_naive(), current.hour(), minute, cx);
        self.picker_minute_scroll
            .scroll_to_top_of_item(minute as usize);
    }

    fn select_picker_today(&mut self, endpoint: SessionRangeEndpoint, cx: &mut Context<Self>) {
        let current = self.endpoint_datetime(endpoint, cx);
        let today = Local::now().date_naive();
        self.picker_year = today.year();
        self.picker_month = today.month();
        self.update_datetime_endpoint(endpoint, today, current.hour(), current.minute(), cx);
    }

    fn clear_picker_value(&mut self, endpoint: SessionRangeEndpoint, cx: &mut Context<Self>) {
        let input = self.endpoint_input(endpoint).clone();
        input.update(cx, |input, cx| input.set_content("", cx));
        self.active_datetime_picker = None;
        cx.notify();
    }

    fn shift_picker_month(&mut self, delta: i32, cx: &mut Context<Self>) {
        let month_index = self.picker_year * 12 + self.picker_month as i32 - 1 + delta;
        self.picker_year = month_index.div_euclid(12);
        self.picker_month = month_index.rem_euclid(12) as u32 + 1;
        cx.notify();
    }

    fn apply_custom_range(&mut self, cx: &mut Context<Self>) {
        let start_text = self.range_start_input.read(cx).content().trim().to_string();
        let end_text = self.range_end_input.read(cx).content().trim().to_string();
        let Some(start) = parse_local_datetime(&start_text, false) else {
            self.date_filter_error = Some(t(k::SESSIONS_FILTER_ERROR_START_INVALID));
            cx.notify();
            return;
        };
        let Some(end) = parse_local_datetime(&end_text, true) else {
            self.date_filter_error = Some(t(k::SESSIONS_FILTER_ERROR_END_INVALID));
            cx.notify();
            return;
        };
        if start > end {
            self.date_filter_error = Some(t(k::SESSIONS_FILTER_ERROR_RANGE_ORDER));
            cx.notify();
            return;
        }
        self.set_date_filter(
            SessionDateFilter::Custom {
                start_ms: start.timestamp_millis(),
                end_ms: end.timestamp_millis(),
            },
            cx,
        );
    }

    fn title_for(session: &SessionMeta) -> String {
        session
            .title
            .clone()
            .or_else(|| session.summary.clone())
            .unwrap_or_else(|| session.session_id.clone())
    }

    /// Queue a toast with an explicit severity. The level travels with the text
    /// so the toast host never has to guess it from the wording.
    ///
    /// Deliberately does not notify: callers decide when the rest of their state
    /// change is complete.
    fn set_status(&mut self, text: impl Into<SharedString>, level: NotificationLevel) {
        self.status = Some(text.into());
        self.status_level = Some(level);
    }

    fn do_delete(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(idx).cloned() else {
            return;
        };
        let Ok(app) = AppId::parse(&session.provider_id) else {
            self.set_status(
                tf!(
                    k::SESSIONS_STATUS_DELETE_FAILED,
                    error = "invalid session app"
                ),
                NotificationLevel::Error,
            );
            cx.notify();
            return;
        };
        let id = session.session_id.clone();
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result =
                crate::core_async::run(async move { backend.delete_session(&app, &id).await })
                    .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.set_status(t(k::SESSIONS_STATUS_DELETED), NotificationLevel::Success);
                        if idx < this.sessions.len() {
                            this.sessions.remove(idx);
                        }
                        this.rebuild_session_index();
                    }
                    Err(error) => this.set_status(
                        tf!(k::SESSIONS_STATUS_DELETE_FAILED, error = error),
                        NotificationLevel::Error,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn prepare_messages(
        messages: Vec<SessionMessage>,
    ) -> (Vec<PreparedSessionMessage>, SessionStats) {
        let mut stats = SessionStats::default();
        let messages = messages
            .into_iter()
            .map(|message| {
                match message.role.as_str() {
                    "user" => stats.user_messages += 1,
                    "assistant" => stats.assistant_messages += 1,
                    "tool" | "system" => stats.tool_messages += 1,
                    _ => {}
                }
                if let Some(timestamp) = message.ts {
                    stats.first_ts = Some(
                        stats
                            .first_ts
                            .map_or(timestamp, |current| current.min(timestamp)),
                    );
                    stats.last_ts = Some(
                        stats
                            .last_ts
                            .map_or(timestamp, |current| current.max(timestamp)),
                    );
                }
                let (preview, is_long) = Self::message_content(&message.content, false);
                let content = if message.content.trim().is_empty() {
                    t(k::SESSIONS_MESSAGE_EMPTY)
                } else {
                    SharedString::from(message.content)
                };
                PreparedSessionMessage {
                    role: message.role,
                    content,
                    preview,
                    is_long,
                    ts: message.ts,
                }
            })
            .collect();
        (messages, stats)
    }

    /// Load a session's full transcript (background — files can be MBs) and
    /// switch to the detail viewer when it arrives.
    /// Open a transcript, optionally scrolled to a specific message — the one a
    /// search result matched.
    fn open_detail_at(&mut self, idx: usize, focus: Option<usize>, cx: &mut Context<Self>) {
        if self.loading_detail.is_some() {
            return;
        }
        let Some(session) = self.sessions.get(idx).cloned() else {
            return;
        };
        self.loading_detail = Some(idx);
        cx.notify();
        let app = match AppId::parse(&session.provider_id) {
            Ok(app) => app,
            Err(error) => {
                self.loading_detail = None;
                self.set_status(
                    tf!(k::SESSIONS_DETAIL_ERROR_LOAD_FAILED, error = error),
                    NotificationLevel::Error,
                );
                cx.notify();
                return;
            }
        };
        let id = session.session_id.clone();
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let loaded = crate::core_async::run(async move {
                backend
                    .get_session_messages(&app, &id)
                    .await
                    .map(|(meta, messages)| {
                        let prepared = Self::prepare_messages(messages);
                        (meta, prepared)
                    })
            })
            .await;
            this.update(cx, |this, cx| {
                this.loading_detail = None;
                let detail = match loaded {
                    Ok((meta, (messages, stats))) => {
                        // The index stores a position in the full transcript,
                        // but the file may have been trimmed since; ignore a
                        // target that no longer exists rather than scrolling
                        // somewhere arbitrary.
                        let focused_message = focus.filter(|&position| position < messages.len());
                        SessionDetail {
                            meta,
                            messages,
                            stats,
                            error: None,
                            focused_message,
                        }
                    }
                    Err(err) => SessionDetail {
                        meta: session,
                        messages: Vec::new(),
                        stats: SessionStats::default(),
                        error: Some(SharedString::from(tf!(
                            k::SESSIONS_DETAIL_ERROR_LOAD_FAILED,
                            error = err
                        ))),
                        focused_message: None,
                    },
                };
                this.transcript_list_state.reset(detail.messages.len());
                this.expanded_messages.clear();
                let focused_message = detail.focused_message;
                this.detail = Some(detail);
                if let Some(position) = focused_message {
                    this.transcript_list_state.scroll_to_reveal_item(position);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_detail(&mut self, cx: &mut Context<Self>) {
        self.detail = None;
        self.transcript_list_state.reset(0);
        self.expanded_messages.clear();
        cx.notify();
    }

    /// Per-role accent color + soft background for a transcript bubble.
    fn role_colors(role: &str) -> (gpui::Rgba, gpui::Rgba) {
        match role {
            "user" => (theme::accent(), theme::accent_soft()),
            "assistant" => (theme::green(), theme::green_soft()),
            "system" => (theme::muted(), theme::inset()),
            _ => (theme::mauve(), theme::surface_hover()),
        }
    }

    fn role_label(role: &str) -> SharedString {
        match role {
            "user" => t(k::SESSIONS_ROLE_USER),
            "assistant" => t(k::SESSIONS_ROLE_ASSISTANT),
            "system" => t(k::SESSIONS_ROLE_SYSTEM),
            "tool" => t(k::SESSIONS_ROLE_TOOL),
            other => SharedString::from(other.to_string()),
        }
    }

    fn app_label(provider_id: &str) -> SharedString {
        SharedString::from(
            match provider_id {
                "claude" => "Claude Code",
                "codex" => "Codex",
                "gemini" => "Gemini",
                "opencode" => "OpenCode",
                "openclaw" => "OpenClaw",
                "hermes" => "Hermes",
                other => other,
            }
            .to_string(),
        )
    }

    fn active_time(session: &SessionMeta, include_year: bool) -> Option<SharedString> {
        let timestamp = session.last_active_at.or(session.created_at)?;
        Local.timestamp_millis_opt(timestamp).single().map(|time| {
            SharedString::from(
                time.format(if include_year {
                    "%Y-%m-%d %H:%M"
                } else {
                    "%m-%d %H:%M"
                })
                .to_string(),
            )
        })
    }

    fn message_content(content: &str, expanded: bool) -> (SharedString, bool) {
        if content.trim().is_empty() {
            return (t(k::SESSIONS_MESSAGE_EMPTY), false);
        }
        let cutoff = content
            .char_indices()
            .nth(MESSAGE_PREVIEW_CHARS)
            .map(|(byte_index, _)| byte_index);
        let is_long = cutoff.is_some();
        if expanded || !is_long {
            return (SharedString::from(content.to_string()), is_long);
        }
        let preview = &content[..cutoff.unwrap_or(content.len())];
        (SharedString::from(format!("{preview}\n\n…")), true)
    }

    fn toggle_message(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.expanded_messages.remove(&index) {
            self.expanded_messages.insert(index);
        }
        self.transcript_list_state
            .remeasure_items(index..index.saturating_add(1));
        cx.notify();
    }

    fn render_message(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(message) = self
            .detail
            .as_ref()
            .and_then(|detail| detail.messages.get(index))
        else {
            return div().into_any_element();
        };
        let expanded = self.expanded_messages.contains(&index);
        let (accent, soft) = Self::role_colors(&message.role);
        let label = Self::role_label(&message.role);
        let content = if expanded || !message.is_long {
            message.content.clone()
        } else {
            message.preview.clone()
        };
        let timestamp = message.ts.and_then(|timestamp| {
            Local
                .timestamp_millis_opt(timestamp)
                .single()
                .map(|value| SharedString::from(value.format("%H:%M:%S").to_string()))
        });
        let is_trace = matches!(message.role.as_str(), "tool" | "system");
        let is_focused = self
            .detail
            .as_ref()
            .and_then(|detail| detail.focused_message)
            == Some(index);

        div()
            .w_full()
            .pb_2()
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .rounded_lg()
                    .border_1()
                    // The message a search result pointed at: the transcript
                    // scrolls here, and this is what says "this one".
                    .when(is_focused, |bubble| {
                        bubble.border_2().border_color(theme::accent())
                    })
                    .when(!is_focused, |bubble| {
                        bubble.border_color(if is_trace {
                            theme::border()
                        } else {
                            accent.alpha(0.36)
                        })
                    })
                    .bg(if is_trace {
                        theme::inset()
                    } else if message.role == "user" {
                        soft.alpha(0.42)
                    } else {
                        theme::surface()
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(components::status_dot_sized(accent, 6.))
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(soft)
                                    .text_color(accent)
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(SharedString::from(format!("#{}", index + 1))),
                            )
                            .child(div().flex_1())
                            .when_some(timestamp, |row, timestamp| {
                                row.child(
                                    div().text_xs().text_color(theme::muted()).child(timestamp),
                                )
                            }),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .overflow_hidden()
                            .text_color(theme::text())
                            .text_sm()
                            .line_height(px(20.))
                            .child(content),
                    )
                    .when(message.is_long, |card| {
                        card.child(
                            div().flex().flex_row().child(
                                components::button(
                                    SharedString::from(format!("session-message-toggle-{index}")),
                                    if expanded {
                                        t(k::SESSIONS_MESSAGE_COLLAPSE)
                                    } else {
                                        t(k::SESSIONS_MESSAGE_EXPAND)
                                    },
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.toggle_message(index, cx);
                                    },
                                )),
                            ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn detail_metric(label: &'static str, value: impl Into<SharedString>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .min_w(px(92.))
            .child(div().text_xs().text_color(theme::muted()).child(label))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child(value.into()),
            )
    }

    fn duration_label(stats: &SessionStats) -> Option<SharedString> {
        let duration_ms = stats.last_ts?.saturating_sub(stats.first_ts?);
        let seconds = duration_ms / 1_000;
        Some(SharedString::from(if seconds < 60 {
            tf!(k::SESSIONS_DURATION_SECONDS, seconds = seconds)
        } else if seconds < 3_600 {
            tf!(
                k::SESSIONS_DURATION_MINUTES,
                minutes = seconds / 60,
                seconds = seconds % 60
            )
        } else {
            tf!(
                k::SESSIONS_DURATION_HOURS,
                hours = seconds / 3_600,
                minutes = seconds % 3_600 / 60
            )
        }))
    }

    fn render_detail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(detail) = self.detail.as_ref() else {
            return layout::page().into_any_element();
        };
        let title = Self::title_for(&detail.meta);
        let provider = Self::app_label(&detail.meta.provider_id);
        let count = detail.messages.len();
        let error = detail.error.clone();
        let user_messages = detail.stats.user_messages;
        let assistant_messages = detail.stats.assistant_messages;
        let tool_messages = detail.stats.tool_messages;
        let duration = Self::duration_label(&detail.stats);
        let subtitle = match Self::active_time(&detail.meta, true) {
            Some(time) => SharedString::from(tf!(
                k::SESSIONS_DETAIL_SUBTITLE_WITH_TIME,
                count = count,
                time = time
            )),
            None => SharedString::from(tf!(k::SESSIONS_DETAIL_SUBTITLE, count = count)),
        };
        if self.transcript_list_state.item_count() != count {
            self.transcript_list_state.reset(count);
        }

        let body = if let Some(error) = error {
            layout::scroll_body(
                "session-transcript-error",
                &self.empty_scroll,
                layout::content_column().child(components::empty_state(
                    IconName::Message,
                    t(k::SESSIONS_DETAIL_ERROR_TITLE),
                    error,
                    None,
                )),
            )
            .into_any_element()
        } else if count == 0 {
            layout::scroll_body(
                "session-transcript-empty",
                &self.empty_scroll,
                layout::content_column().child(components::empty_state(
                    IconName::Message,
                    t(k::SESSIONS_DETAIL_EMPTY_TITLE),
                    t(k::SESSIONS_DETAIL_EMPTY_HINT),
                    None,
                )),
            )
            .into_any_element()
        } else {
            let list = gpui::list(
                self.transcript_list_state.clone(),
                cx.processor(|this, index, _window, cx| this.render_message(index, cx)),
            );
            layout::virtual_body("session-transcript-body", list, &self.transcript_list_state)
                .into_any_element()
        };

        let metrics = components::card().p_3().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_5()
                .child(Self::detail_metric(
                    raw(k::SESSIONS_METRIC_MESSAGES),
                    count.to_string(),
                ))
                .child(Self::detail_metric(
                    raw(k::SESSIONS_METRIC_USER),
                    user_messages.to_string(),
                ))
                .child(Self::detail_metric(
                    raw(k::SESSIONS_METRIC_ASSISTANT),
                    assistant_messages.to_string(),
                ))
                .child(Self::detail_metric(
                    raw(k::SESSIONS_METRIC_TOOL_SYSTEM),
                    tool_messages.to_string(),
                ))
                .when_some(duration, |row, duration| {
                    row.child(Self::detail_metric(
                        raw(k::SESSIONS_METRIC_DURATION),
                        duration,
                    ))
                }),
        );

        layout::page()
            .child(
                layout::page_header(title, Some(subtitle)).child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .flex_shrink_0()
                        .child(
                            components::icon_button_tone(
                                "session-back",
                                t(k::SESSIONS_DETAIL_BACK),
                                IconName::ChevronLeft,
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(
                                cx.listener(|this, _event, _window, cx| this.close_detail(cx)),
                            ),
                        )
                        .child(components::badge(BadgeTone::Teal, provider)),
                ),
            )
            .child(
                div()
                    .px_6()
                    .pt_4()
                    .child(layout::content_column().child(metrics)),
            )
            .child(body)
            .into_any_element()
    }

    fn render_card(
        &self,
        idx: usize,
        session: &SessionMeta,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let title = Self::title_for(session);
        let provider = Self::app_label(&session.provider_id);
        let active_time = Self::active_time(session, false);
        let is_loading = self.loading_detail == Some(idx);
        let hit = self.content_hit_for(session);
        let snippet = hit.map(|hit| SharedString::from(hit.snippet.clone()));
        // Clicking a session that matched on its transcript should land on the
        // matching message, not the top of a thousand-message log.
        let focus = hit.map(|hit| hit.ord);

        components::card()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .id(SharedString::from(format!("session-open-{idx}")))
                    .role(gpui::Role::Button)
                    .aria_label(t(k::SESSIONS_CARD_OPEN_ARIA))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .flex_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.open_detail_at(idx, focus, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(components::badge(BadgeTone::Teal, provider))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(SharedString::from(title)),
                            ),
                    )
                    .when_some(snippet, |s, snippet| {
                        s.child(
                            div()
                                .min_w_0()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1p5()
                                .child(icon(IconName::Search, theme::accent(), 12.))
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme::subtext())
                                        .child(snippet),
                                ),
                        )
                    })
                    .when_some(active_time, |s, time| {
                        s.child(
                            div()
                                .min_w_0()
                                .text_color(theme::muted())
                                .text_xs()
                                .child(time),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        components::button(
                            SharedString::from(format!("session-view-{idx}")),
                            if is_loading {
                                t(k::SESSIONS_CARD_LOADING)
                            } else {
                                t(k::SESSIONS_CARD_VIEW)
                            },
                            if is_loading {
                                ButtonTone::Neutral
                            } else {
                                ButtonTone::Primary
                            },
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.open_detail_at(idx, focus, cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("session-delete-{idx}")),
                            t(k::SESSIONS_ACTION_DELETE),
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm_delete = Some(idx);
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    fn render_session_list_item(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(session_index) = self.visible_session_indices.get(index).copied() else {
            return div().into_any_element();
        };
        let Some(session) = self.sessions.get(session_index) else {
            return div().into_any_element();
        };
        div()
            .w_full()
            .pb_3()
            .child(self.render_card(session_index, session, cx))
            .into_any_element()
    }

    fn render_datetime_picker(
        &self,
        endpoint: SessionRangeEndpoint,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let selected = self.endpoint_datetime(endpoint, cx);
        let picker_id = match endpoint {
            SessionRangeEndpoint::Start => "sessions-start-datetime",
            SessionRangeEndpoint::End => "sessions-end-datetime",
        };
        let shift_month = cx.listener(|this, delta: &i32, _window, cx| {
            this.shift_picker_month(*delta, cx);
        });
        let select_date = cx.listener(move |this, date: &NaiveDate, _window, cx| {
            this.select_picker_date(endpoint, *date, cx);
        });
        let select_hour = cx.listener(move |this, hour: &u32, _window, cx| {
            this.select_picker_hour(endpoint, *hour, cx);
        });
        let select_minute = cx.listener(move |this, minute: &u32, _window, cx| {
            this.select_picker_minute(endpoint, *minute, cx);
        });
        let select_today = cx.listener(move |this, _event: &(), _window, cx| {
            this.select_picker_today(endpoint, cx);
        });
        let clear = cx.listener(move |this, _event: &(), _window, cx| {
            this.clear_picker_value(endpoint, cx);
        });

        components::datetime_picker(
            picker_id,
            selected,
            self.picker_year,
            self.picker_month,
            &self.picker_hour_scroll,
            &self.picker_minute_scroll,
            move |delta, window, cx| shift_month(&delta, window, cx),
            move |date, window, cx| select_date(&date, window, cx),
            move |hour, window, cx| select_hour(&hour, window, cx),
            move |minute, window, cx| select_minute(&minute, window, cx),
            move |window, cx| select_today(&(), window, cx),
            move |window, cx| clear(&(), window, cx),
        )
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let date_open = self.open_filter_popover == Some(SessionFilterPopover::Date);
        let start_picker_open = self.active_datetime_picker == Some(SessionRangeEndpoint::Start);
        let start_control = div()
            .relative()
            .w_full()
            .child(
                components::datetime_filter_field(
                    "sessions-start-datetime-field",
                    raw(k::SESSIONS_FILTER_DATE_START_LABEL),
                    self.range_start_input.clone(),
                    start_picker_open,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_datetime_picker(SessionRangeEndpoint::Start, cx);
                    }),
                ),
            )
            .when(start_picker_open, |control| {
                control.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(self.render_datetime_picker(SessionRangeEndpoint::Start, cx)),
                    )
                    .priority(20),
                )
            });
        let end_picker_open = self.active_datetime_picker == Some(SessionRangeEndpoint::End);
        let end_control = div()
            .relative()
            .w_full()
            .child(
                components::datetime_filter_field(
                    "sessions-end-datetime-field",
                    raw(k::SESSIONS_FILTER_DATE_END_LABEL),
                    self.range_end_input.clone(),
                    end_picker_open,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_datetime_picker(SessionRangeEndpoint::End, cx);
                    }),
                ),
            )
            .when(end_picker_open, |control| {
                control.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(self.render_datetime_picker(SessionRangeEndpoint::End, cx)),
                    )
                    .priority(20),
                )
            });
        let mut date_popover = session_filter_popover("sessions-date-popover", 380.)
            .p_1()
            .child(
                session_dropdown_option(
                    "sessions-date-all",
                    t(k::SESSIONS_FILTER_DATE_ALL),
                    self.date_filter == SessionDateFilter::All,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_date_filter(SessionDateFilter::All, cx);
                })),
            )
            .child(
                session_dropdown_option(
                    "sessions-date-today",
                    t(k::SESSIONS_FILTER_DATE_TODAY),
                    self.date_filter == SessionDateFilter::Today,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_date_filter(SessionDateFilter::Today, cx);
                })),
            )
            .child(
                session_dropdown_option(
                    "sessions-date-week",
                    t(k::SESSIONS_FILTER_DATE_SEVEN_DAYS),
                    self.date_filter == SessionDateFilter::SevenDays,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_date_filter(SessionDateFilter::SevenDays, cx);
                })),
            )
            .child(
                session_dropdown_option(
                    "sessions-date-month",
                    t(k::SESSIONS_FILTER_DATE_THIRTY_DAYS),
                    self.date_filter == SessionDateFilter::ThirtyDays,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_date_filter(SessionDateFilter::ThirtyDays, cx);
                })),
            )
            .child(div().mx_2().my_1().h(px(1.)).bg(theme::border()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_3()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::muted())
                            .child(t(k::SESSIONS_FILTER_DATE_CUSTOM_RANGE)),
                    )
                    .child(start_control)
                    .child(end_control)
                    .when_some(self.date_filter_error.clone(), |column, error| {
                        column.child(div().text_xs().text_color(theme::red()).child(error))
                    })
                    .child(
                        components::button(
                            "sessions-date-apply",
                            t(k::SESSIONS_FILTER_DATE_APPLY),
                            ButtonTone::Primary,
                            ButtonSize::Sm,
                        )
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.apply_custom_range(cx);
                            },
                        )),
                    ),
            );
        date_popover = date_popover.when(self.active_datetime_picker.is_none(), |popover| {
            popover.on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                if this.open_filter_popover == Some(SessionFilterPopover::Date) {
                    this.open_filter_popover = None;
                    this.date_filter_error = None;
                    cx.notify();
                }
            }))
        });
        let date_control = div()
            .relative()
            .flex_none()
            .child(
                session_filter_trigger(
                    "sessions-date-filter",
                    self.date_filter.label(),
                    IconName::Calendar,
                    date_open,
                    220.,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.toggle_filter_popover(SessionFilterPopover::Date, cx);
                    }),
                ),
            )
            .when(date_open, |control| {
                control.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(date_popover),
                    )
                    .priority(10),
                )
            });

        let app_open = self.open_filter_popover == Some(SessionFilterPopover::App);
        let mut app_options = div()
            .id("sessions-app-options")
            .max_h(px(280.))
            .overflow_y_scroll()
            .track_scroll(&self.app_filter_scroll)
            .on_scroll_wheel(crate::scrollbar::contain_vertical_scroll(
                self.app_filter_scroll.clone(),
            ))
            .p_1()
            .child(
                session_dropdown_option(
                    "sessions-app-all",
                    t(k::SESSIONS_FILTER_APP_ALL),
                    self.app_filter.is_none(),
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_app_filter(None, cx);
                })),
            );
        for (index, app) in self.app_options.iter().cloned().enumerate() {
            let selected = self.app_filter.as_deref() == Some(app.as_str());
            let app_for_click = app.clone();
            app_options = app_options.child(
                session_dropdown_option(
                    ElementId::Name(format!("sessions-app-option-{index}").into()),
                    Self::app_label(&app),
                    selected,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.set_app_filter(Some(app_for_click.clone()), cx);
                })),
            );
        }
        let app_popover = session_filter_popover("sessions-app-popover", 220.)
            .relative()
            .p_0()
            .child(app_options)
            .child(crate::scrollbar::VerticalScrollbar::new(
                "sessions-app-options-scrollbar",
                self.app_filter_scroll.clone(),
            ))
            .on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                if this.open_filter_popover == Some(SessionFilterPopover::App) {
                    this.open_filter_popover = None;
                    cx.notify();
                }
            }));
        let app_label = self
            .app_filter
            .as_deref()
            .map(Self::app_label)
            .unwrap_or_else(|| t(k::SESSIONS_FILTER_APP_ALL));
        let app_control = div()
            .relative()
            .flex_none()
            .child(
                session_filter_trigger(
                    "sessions-app-filter",
                    app_label,
                    IconName::Layers,
                    app_open,
                    176.,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.toggle_filter_popover(SessionFilterPopover::App, cx);
                    }),
                ),
            )
            .when(app_open, |control| {
                control.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(app_popover),
                    )
                    .priority(10),
                )
            });

        let has_active_filters = self.date_filter != SessionDateFilter::All
            || self.app_filter.is_some()
            || !self.query.is_empty();

        components::card()
            .p_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(180.))
                            .child(self.search_input.clone()),
                    )
                    .child(date_control)
                    .child(app_control)
                    .when(has_active_filters, |row| {
                        row.child(
                            components::button(
                                "sessions-clear-filters",
                                t(k::SESSIONS_FILTER_RESET),
                                ButtonTone::Ghost,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.clear_filters(cx);
                                },
                            )),
                        )
                    }),
            )
            .when_some(self.render_search_hint(cx), |card, hint| card.child(hint))
    }

    /// The line under the search box that says how far the search reached.
    ///
    /// With the index off, a search still runs — over titles alone — so this is
    /// where that limit is stated, along with the way out of it. Saying nothing
    /// would leave the user to conclude the session simply is not there.
    fn render_search_hint(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let (icon_name, tone, text): (IconName, gpui::Rgba, SharedString) = match &self.index_state
        {
            IndexSyncState::Failed(error) => (
                IconName::Search,
                theme::red(),
                SharedString::from(tf!(
                    k::SESSIONS_SEARCH_INDEX_FAILED,
                    error = error.to_string()
                )),
            ),
            IndexSyncState::Syncing { done, total, .. } => (
                IconName::Refresh,
                theme::muted(),
                SharedString::from(tf!(
                    k::SESSIONS_SEARCH_INDEXING,
                    done = done.to_string(),
                    total = total.to_string()
                )),
            ),
            IndexSyncState::Idle => {
                if self.index_enabled {
                    if !self.searching {
                        return None;
                    }
                    (IconName::Search, theme::muted(), t(k::SESSIONS_SEARCH_BUSY))
                } else {
                    // Only worth saying once the user is actually searching.
                    if self.query.is_empty() {
                        return None;
                    }
                    (
                        IconName::Search,
                        theme::muted(),
                        t(k::SESSIONS_SEARCH_TITLES_ONLY),
                    )
                }
            }
        };

        let enable = !self.backend.is_remote()
            && !self.index_enabled
            && matches!(self.index_state, IndexSyncState::Idle);
        Some(
            div()
                .pt_2()
                .flex()
                .flex_row()
                .items_center()
                .gap_1p5()
                .child(icon(icon_name, tone, 12.))
                .child(div().text_xs().text_color(tone).child(text))
                .when(enable, |row| {
                    row.child(
                        components::button(
                            "sessions-enable-index",
                            t(k::SESSIONS_SEARCH_ENABLE_INDEX),
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.enable_index(cx);
                            },
                        )),
                    )
                }),
        )
    }

    /// Switch the index on from the Sessions panel and start filling it.
    fn enable_index(&mut self, cx: &mut Context<Self>) {
        if !self.workspace_available {
            return;
        }
        if self.backend.is_remote() {
            let backend = self.backend.clone();
            cx.spawn(async move |this, cx| {
                let result = crate::core_async::run(async move {
                    backend
                        .set_setting("sessionIndexEnabled", serde_json::json!(true))
                        .await?;
                    backend
                        .set_setting("sessionIndexDisabledAt", serde_json::Value::Null)
                        .await?;
                    Ok::<_, crate::remote::WorkspaceBackendError>(())
                })
                .await;
                this.update(cx, |this, cx| match result {
                    Ok(()) => {
                        this.index_enabled = true;
                        this.start_index_sync(cx);
                        cx.notify();
                    }
                    Err(error) => {
                        this.set_status(
                            tf!(k::SESSIONS_SEARCH_INDEX_FAILED, error = error),
                            NotificationLevel::Error,
                        );
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
            return;
        }
        let mut settings = ochub_core::settings::get_settings();
        settings.session_index_enabled = true;
        settings.session_index_disabled_at = None;
        if let Err(error) = ochub_core::settings::update_settings(settings) {
            self.set_status(
                tf!(k::SESSIONS_SEARCH_INDEX_FAILED, error = error.to_string()),
                NotificationLevel::Error,
            );
            cx.notify();
            return;
        }
        self.index_enabled = true;
        self.start_index_sync(cx);
        cx.notify();
    }

    fn render_pagination(&self, cx: &mut Context<Self>) -> gpui::Div {
        let go = cx.listener(|this, page: &u32, _window, cx| {
            this.set_page(*page as usize, cx);
        });
        let toggle_page_size = cx.listener(|this, _event: &(), _window, cx| {
            this.toggle_page_size(cx);
        });
        let set_page_size = cx.listener(|this, page_size: &u32, _window, cx| {
            this.set_page_size(*page_size as usize, cx);
        });
        div().px_6().child(components::pagination_bar(
            "sessions-pages",
            self.page as u32,
            self.total_pages() as u32,
            Some(self.filtered_session_count() as u64),
            self.page_size as u32,
            PAGE_SIZE_OPTIONS,
            self.page_size_open,
            &self.page_input,
            move |page, window, cx| go(&page, window, cx),
            move |window, cx| toggle_page_size(&(), window, cx),
            move |page_size, window, cx| set_page_size(&page_size, window, cx),
        ))
    }
}

fn session_filter_trigger(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon_name: IconName,
    expanded: bool,
    width: f32,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .aria_expanded(expanded)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w(px(width))
        .h(px(36.))
        .px_3()
        .rounded_lg()
        .border_1()
        .border_color(if expanded {
            theme::accent()
        } else {
            theme::border_strong()
        })
        .bg(theme::surface())
        .cursor_pointer()
        .text_sm()
        .text_color(theme::text())
        .hover(|style| style.border_color(theme::accent()).bg(theme::panel()))
        .child(icon(icon_name, theme::muted(), 15.))
        .child(div().min_w_0().flex_1().truncate().child(label))
        .child(icon(IconName::ChevronDown, theme::muted(), 13.))
}

fn session_filter_popover(id: &'static str, width: f32) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .w(px(width))
        .rounded_lg()
        .border_1()
        .border_color(theme::border())
        .bg(theme::overlay())
        .shadow(theme::shadow_popover())
        .occlude()
}

fn session_dropdown_option(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    let option = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .aria_selected(selected)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .min_h(px(34.))
        .px_3()
        .py_1p5()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .child(div().min_w_0().flex_1().truncate().child(label));
    if selected {
        option
            .bg(theme::accent_soft())
            .text_color(theme::accent())
            .font_weight(FontWeight::MEDIUM)
            .child(icon(IconName::Check, theme::accent(), 13.))
    } else {
        option
            .text_color(theme::subtext())
            .hover(|style| style.bg(theme::surface_hover()).text_color(theme::text()))
    }
}

fn text_input(cx: &mut Context<TextInput>, placeholder: impl Into<SharedString>) -> TextInput {
    TextInput::new(cx, placeholder)
}

/// Progress to show on the waiting page, or `None` to render the list.
///
/// Only a *cold* build earns the page: until it finishes, search cannot answer
/// anything, so there is something to wait for. An incremental pass leaves both
/// the list and search working and stays the one-line hint under the search box
/// instead — a routine resync must never take the panel away.
fn index_wait_progress(state: &IndexSyncState, dismissed: bool) -> Option<(usize, usize)> {
    if dismissed {
        return None;
    }
    match state {
        IndexSyncState::Syncing { done, total, cold } if *cold => Some((*done, *total)),
        _ => None,
    }
}

/// Whether a session matches on the metadata the scan already loaded.
///
/// This is the half of search that needs no index, so it is also what the
/// search box falls back to when the index is switched off. `needle` is
/// expected to be lowercase already — it is lowered once per query rather than
/// once per session per frame.
fn metadata_matches(session: &SessionMeta, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let contains = |value: &str| value.to_lowercase().contains(needle);
    session.title.as_deref().is_some_and(contains)
        || session.summary.as_deref().is_some_and(contains)
        || session.project_dir.as_deref().is_some_and(contains)
        || contains(&session.session_id)
}

fn parse_local_datetime(value: &str, end_of_day: bool) -> Option<chrono::DateTime<Local>> {
    let normalized = value.trim().replace('/', "-");
    for pattern in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(&normalized, pattern) {
            return Local.from_local_datetime(&value).earliest();
        }
    }

    let date = components::parse_jump_date(&normalized)?;
    let value = if end_of_day {
        date.and_hms_opt(23, 59, 59)?
    } else {
        date.and_hms_opt(0, 0, 0)?
    };
    Local.from_local_datetime(&value).earliest()
}

impl Render for SessionsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.detail.is_some() {
            return self.render_detail(cx);
        }
        let total = self.filtered_session_count();
        let has_no_sessions = self.sessions.is_empty();
        let has_no_matches = !has_no_sessions && total == 0;
        let scanning = self.scanning;
        let index_wait = self.index_wait_progress();
        let show_pagination = total > 0 && index_wait.is_none();
        let confirm = self.confirm_delete.and_then(|idx| {
            self.sessions
                .get(idx)
                .map(Self::title_for)
                .map(|t| (idx, t))
        });
        let body = if let Some((done, indexed_total)) = index_wait {
            self.render_index_wait(done, indexed_total, cx)
        } else if has_no_sessions {
            layout::scroll_body(
                "session-empty-body",
                &self.empty_scroll,
                layout::content_column().child(components::empty_state(
                    IconName::Clock,
                    if scanning {
                        t(k::SESSIONS_EMPTY_SCANNING_TITLE)
                    } else {
                        t(k::SESSIONS_EMPTY_NONE_TITLE)
                    },
                    if scanning {
                        t(k::SESSIONS_EMPTY_SCANNING_HINT)
                    } else {
                        t(k::SESSIONS_EMPTY_NONE_HINT)
                    },
                    None,
                )),
            )
            .into_any_element()
        } else if has_no_matches {
            layout::scroll_body(
                "session-no-matches-body",
                &self.empty_scroll,
                layout::content_column().child(components::empty_state(
                    IconName::Search,
                    t(k::SESSIONS_EMPTY_NO_MATCHES_TITLE),
                    t(k::SESSIONS_EMPTY_NO_MATCHES_HINT),
                    Some(
                        components::button(
                            "sessions-empty-clear-filters",
                            t(k::SESSIONS_EMPTY_NO_MATCHES_CLEAR),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.clear_filters(cx);
                        }))
                        .into_any_element(),
                    ),
                )),
            )
            .into_any_element()
        } else {
            let list = gpui::list(
                self.session_list_state.clone(),
                cx.processor(|this, index, _window, cx| this.render_session_list_item(index, cx)),
            );
            layout::virtual_body("session-list-body", list, &self.session_list_state)
                .into_any_element()
        };

        layout::page()
            .relative()
            .child(
                layout::page_header(t(k::SESSIONS_HEADER_TITLE), None).child(
                    components::icon_button_tone(
                        "sessions-refresh",
                        if scanning {
                            t(k::SESSIONS_HEADER_SCANNING)
                        } else {
                            t(k::SESSIONS_HEADER_REFRESH)
                        },
                        IconName::Refresh,
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .flex_shrink_0()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.force_reload(cx);
                    })),
                ),
            )
            // The filter row is hidden behind the waiting page: with the index
            // still empty the search box could only answer on titles, which is
            // exactly the half-answer the page exists to avoid.
            .when(index_wait.is_none(), |page| {
                page.child(
                    div()
                        .px_6()
                        .pt_6()
                        .child(layout::content_column().child(self.render_filters(cx))),
                )
            })
            .child(body)
            .when(show_pagination, |s| s.child(self.render_pagination(cx)))
            .when_some(confirm, |root, (idx, title)| {
                let message =
                    SharedString::from(tf!(k::SESSIONS_CONFIRM_DELETE_MESSAGE, title = title));
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(t(
                            k::SESSIONS_CONFIRM_DELETE_TITLE,
                        )))
                        .child(
                            components::modal_body()
                                .child(div().text_color(theme::subtext()).text_sm().child(message)),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "session-confirm-delete-cancel",
                                t(k::SESSIONS_CONFIRM_DELETE_CANCEL),
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm_delete = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "session-confirm-delete-ok",
                                t(k::SESSIONS_ACTION_DELETE),
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm_delete = None;
                                this.do_delete(idx, cx);
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
            .into_any_element()
    }
}

crate::notifications::impl_status_toasts_leveled!(SessionsView);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_date_filter_uses_inclusive_local_range() {
        let start = Local
            .with_ymd_and_hms(2026, 7, 22, 9, 0, 0)
            .single()
            .expect("valid local time")
            .timestamp_millis();
        let end = Local
            .with_ymd_and_hms(2026, 7, 22, 17, 0, 0)
            .single()
            .expect("valid local time")
            .timestamp_millis();
        let filter = SessionDateFilter::Custom {
            start_ms: start,
            end_ms: end,
        };

        assert!(filter.matches(Some(start)));
        assert!(filter.matches(Some(end)));
        assert!(!filter.matches(Some(start - 1)));
        assert!(!filter.matches(Some(end + 1)));
        assert!(SessionDateFilter::All.matches(None));
        assert!(!SessionDateFilter::Today.matches(None));
    }

    #[test]
    fn local_datetime_parser_supports_dates_and_minutes() {
        let start = parse_local_datetime("2026-07-22", false).expect("valid start");
        let end = parse_local_datetime("2026/07/22", true).expect("valid end");
        let minute = parse_local_datetime("2026-07-22 12:34", false).expect("valid minute");

        assert_eq!((start.hour(), start.minute(), start.second()), (0, 0, 0));
        assert_eq!((end.hour(), end.minute(), end.second()), (23, 59, 59));
        assert_eq!((minute.hour(), minute.minute()), (12, 34));
        assert!(parse_local_datetime("2026-02-30", false).is_none());
    }

    #[test]
    fn long_messages_are_collapsed_on_unicode_boundaries() {
        let content = "你".repeat(MESSAGE_PREVIEW_CHARS + 20);
        let (preview, is_long) = SessionsView::message_content(&content, false);
        assert!(is_long);
        assert!(preview.ends_with('…'));
        assert!(preview.len() < content.len());

        let (expanded, is_long) = SessionsView::message_content(&content, true);
        assert!(is_long);
        assert_eq!(expanded.as_ref(), content);
    }

    #[test]
    fn short_messages_are_not_marked_as_collapsed() {
        let (content, is_long) = SessionsView::message_content("hello", false);
        assert!(!is_long);
        assert_eq!(content.as_ref(), "hello");
    }

    fn session(title: Option<&str>, project_dir: Option<&str>) -> SessionMeta {
        SessionMeta {
            provider_id: "codex".to_string(),
            session_id: "0f9c-ABCD".to_string(),
            title: title.map(str::to_string),
            summary: Some("A summary about caching".to_string()),
            project_dir: project_dir.map(str::to_string),
            created_at: Some(1),
            last_active_at: Some(1),
            source_path: Some("/tmp/session.jsonl".to_string()),
            resume_command: None,
        }
    }

    #[test]
    fn metadata_search_covers_the_fields_a_scan_already_loaded() {
        let session = session(Some("Refactor the RelayStation"), Some("/code/OcHub"));

        assert!(metadata_matches(&session, "relaystation"));
        assert!(
            metadata_matches(&session, "caching"),
            "summary should match"
        );
        assert!(
            metadata_matches(&session, "ochub"),
            "project dir should match"
        );
        assert!(
            metadata_matches(&session, "0f9c"),
            "session id should match"
        );
        assert!(!metadata_matches(&session, "nothing here"));
    }

    #[test]
    fn an_empty_query_matches_every_session() {
        // The filter runs on every row; a blank search box must not narrow it.
        assert!(metadata_matches(&session(None, None), ""));
    }

    #[test]
    fn only_a_cold_build_takes_over_the_panel() {
        let cold = IndexSyncState::Syncing {
            done: 12,
            total: 400,
            cold: true,
        };
        assert_eq!(index_wait_progress(&cold, false), Some((12, 400)));

        // An incremental pass runs behind a working list and search.
        let warm = IndexSyncState::Syncing {
            done: 12,
            total: 400,
            cold: false,
        };
        assert_eq!(index_wait_progress(&warm, false), None);

        assert_eq!(index_wait_progress(&IndexSyncState::Idle, false), None);
        assert_eq!(
            index_wait_progress(&IndexSyncState::Failed("boom".into()), false),
            None,
            "a failed sync should show its error over the list, not a wait"
        );
    }

    #[test]
    fn dismissing_the_wait_returns_the_list_while_the_build_runs_on() {
        let cold = IndexSyncState::Syncing {
            done: 12,
            total: 400,
            cold: true,
        };
        assert_eq!(index_wait_progress(&cold, true), None);
    }

    #[test]
    fn metadata_search_tolerates_missing_fields() {
        let session = session(None, None);
        assert!(metadata_matches(&session, "caching"));
        assert!(!metadata_matches(&session, "relaystation"));
    }
}

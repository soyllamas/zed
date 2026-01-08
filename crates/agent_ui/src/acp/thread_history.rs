use crate::{RemoveHistory, RemoveSelectedThread};
use acp_thread::{AgentSessionInfo, AgentSessionList, SessionListParams};
use agent_client_protocol as acp;
use chrono::{Datelike as _, Local, NaiveDate, TimeDelta, Utc};
use editor::{Editor, EditorEvent};
use fuzzy::StringMatchCandidate;
use gpui::{
    App, BackgroundExecutor, Entity, EventEmitter, FocusHandle, Focusable, ScrollStrategy, Task,
    UniformListScrollHandle, Window, uniform_list,
};
use std::{fmt::Display, ops::Range, rc::Rc};
use text::Bias;
use time::{OffsetDateTime, UtcOffset};
use ui::{
    HighlightedLabel, IconButtonShape, ListItem, ListItemSpacing, Tab, Tooltip, WithScrollbar,
    prelude::*,
};
use util::ResultExt as _;

pub struct AcpThreadHistory {
    session_list: Rc<dyn AgentSessionList>,
    delete_supported: bool,
    delete_all_supported: bool,
    scroll_handle: UniformListScrollHandle,
    selected_index: usize,
    hovered_index: Option<usize>,
    search_editor: Entity<Editor>,
    search_query: SharedString,
    visible_items: Vec<ListItemType>,
    local_timezone: UtcOffset,
    confirming_delete_history: bool,
    _update_task: Task<()>,
    _subscriptions: Vec<gpui::Subscription>,
}

enum ListItemType {
    BucketSeparator(TimeBucket),
    Entry {
        entry: AgentSessionInfo,
        format: EntryTimeFormat,
    },
    SearchResult {
        entry: AgentSessionInfo,
        positions: Vec<usize>,
    },
}

impl ListItemType {
    fn session(&self) -> Option<&AgentSessionInfo> {
        match self {
            ListItemType::Entry { entry, .. } => Some(entry),
            ListItemType::SearchResult { entry, .. } => Some(entry),
            _ => None,
        }
    }
}

pub enum ThreadHistoryEvent {
    OpenSession(acp::SessionId),
}

impl EventEmitter<ThreadHistoryEvent> for AcpThreadHistory {}

impl AcpThreadHistory {
    pub(crate) fn new(
        session_list: Rc<dyn AgentSessionList>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search threads...", window, cx);
            editor
        });

        let search_editor_subscription =
            cx.subscribe(&search_editor, |this, search_editor, event, cx| {
                if let EditorEvent::BufferEdited = event {
                    let query = search_editor.read(cx).text(cx);
                    if this.search_query != query {
                        this.search_query = query.into();
                        this.update_visible_items(false, cx);
                    }
                }
            });

        let scroll_handle = UniformListScrollHandle::default();

        let delete_supported = session_list.supports_delete();
        let delete_all_supported = session_list.delete_all_sessions(cx).is_some();

        let local_timezone =
            UtcOffset::from_whole_seconds(chrono::Local::now().offset().local_minus_utc())
                .unwrap_or_else(|_| UtcOffset::UTC);

        let mut this = Self {
            session_list,
            delete_supported,
            delete_all_supported,
            scroll_handle,
            selected_index: 0,
            hovered_index: None,
            visible_items: Default::default(),
            search_editor,
            local_timezone,
            search_query: SharedString::default(),
            confirming_delete_history: false,
            _subscriptions: vec![search_editor_subscription],
            _update_task: Task::ready(()),
        };
        this.update_visible_items(false, cx);
        this
    }

    pub(crate) fn set_session_list(
        &mut self,
        session_list: Rc<dyn AgentSessionList>,
        cx: &mut Context<Self>,
    ) {
        self.delete_supported = session_list.supports_delete();
        self.delete_all_supported = session_list.delete_all_sessions(cx).is_some();
        self.session_list = session_list;
        self.update_visible_items(true, cx);
        cx.notify();
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.update_visible_items(true, cx);
        cx.notify();
    }

    fn update_visible_items(&mut self, preserve_selected_item: bool, cx: &mut Context<Self>) {
        let list_task = self.session_list.list(SessionListParams::default(), cx);

        let query = self.search_query.clone();
        let selected_session_id = if preserve_selected_item {
            self.selected_session_id().cloned()
        } else {
            None
        };

        self._update_task = cx.spawn(async move |this, cx| {
            let result = match list_task.await {
                Ok(result) => result,
                Err(err) => {
                    log::error!("Failed to list agent sessions: {err:?}");
                    return;
                }
            };

            let mut sessions = result.sessions;
            sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

            let query = query.clone();
            let background_executor = cx.background_executor().clone();

            let new_visible_items = cx
                .background_spawn(async move {
                    if query.is_empty() {
                        build_bucketed_items(sessions)
                    } else {
                        build_search_items(sessions, query, background_executor).await
                    }
                })
                .await;

            if let Err(err) = this.update(cx, |this, cx| {
                let new_selected_index = if let Some(session_id) = selected_session_id {
                    new_visible_items
                        .iter()
                        .position(|visible_entry| {
                            visible_entry
                                .session()
                                .is_some_and(|entry| entry.session_id == session_id)
                        })
                        .unwrap_or(0)
                } else {
                    0
                };

                this.visible_items = new_visible_items;
                this.set_selected_index(new_selected_index, Bias::Right, cx);
                cx.notify();
            }) {
                log::error!("Failed to update agent session history UI: {err:?}");
            }
        });
    }

    fn search_produced_no_matches(&self) -> bool {
        self.visible_items.is_empty() && !self.search_query.is_empty()
    }

    fn selected_session_id(&self) -> Option<&acp::SessionId> {
        self.get_session(self.selected_index)
            .map(|session| &session.session_id)
    }

    fn get_session(&self, visible_items_ix: usize) -> Option<&AgentSessionInfo> {
        self.visible_items.get(visible_items_ix)?.session()
    }

    fn set_selected_index(&mut self, mut index: usize, bias: Bias, cx: &mut Context<Self>) {
        if self.visible_items.len() == 0 {
            self.selected_index = 0;
            return;
        }
        while matches!(
            self.visible_items.get(index),
            None | Some(ListItemType::BucketSeparator(..))
        ) {
            index = match bias {
                Bias::Left => {
                    if index == 0 {
                        self.visible_items.len() - 1
                    } else {
                        index - 1
                    }
                }
                Bias::Right => {
                    if index >= self.visible_items.len() - 1 {
                        0
                    } else {
                        index + 1
                    }
                }
            };
        }
        self.selected_index = index;
        self.scroll_handle
            .scroll_to_item(index, ScrollStrategy::Top);
        cx.notify()
    }

    pub fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_index == 0 {
            self.set_selected_index(self.visible_items.len() - 1, Bias::Left, cx);
        } else {
            self.set_selected_index(self.selected_index - 1, Bias::Left, cx);
        }
    }

    pub fn select_next(
        &mut self,
        _: &menu::SelectNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_index == self.visible_items.len() - 1 {
            self.set_selected_index(0, Bias::Right, cx);
        } else {
            self.set_selected_index(self.selected_index + 1, Bias::Right, cx);
        }
    }

    fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_selected_index(0, Bias::Right, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_selected_index(self.visible_items.len() - 1, Bias::Left, cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_entry(self.selected_index, cx);
    }

    fn confirm_entry(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(session) = self.get_session(ix) else {
            return;
        };
        cx.emit(ThreadHistoryEvent::OpenSession(session.session_id.clone()));
    }

    fn remove_selected_thread(
        &mut self,
        _: &RemoveSelectedThread,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remove_thread(self.selected_index, cx)
    }

    fn remove_thread(&mut self, visible_item_ix: usize, cx: &mut Context<Self>) {
        if !self.delete_supported {
            return;
        }

        let Some(session) = self.get_session(visible_item_ix) else {
            return;
        };

        let Some(deleter) = self.session_list.delete_session(&session.session_id, cx) else {
            return;
        };

        let delete_task = deleter.run(cx);
        cx.spawn(async move |this, cx| {
            if let Err(err) = delete_task.await {
                log::error!("Failed to delete agent session: {err:?}");
                return;
            }

            this.update(cx, |this, cx| {
                this.update_visible_items(true, cx);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn remove_history(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(deleter) = self.session_list.delete_all_sessions(cx) else {
            self.confirming_delete_history = false;
            cx.notify();
            return;
        };

        let delete_task = deleter.run(cx);
        self.confirming_delete_history = false;

        cx.spawn(async move |this, cx| {
            if let Err(err) = delete_task.await {
                log::error!("Failed to delete all agent sessions: {err:?}");
                return;
            }

            // Recompute visible items by re-running the normal listing flow.
            // This keeps selection logic and search/bucketing behavior consistent.
            this.update(cx, |this, cx| {
                this.update_visible_items(false, cx);
                cx.notify();
            })
            .log_err();
        })
        .detach();

        cx.notify();
    }

    fn prompt_delete_history(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.confirming_delete_history = true;
        cx.notify();
    }

    fn cancel_delete_history(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.confirming_delete_history = false;
        cx.notify();
    }

    fn render_list_items(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.visible_items
            .get(range.clone())
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(ix, item)| self.render_list_item(item, range.start + ix, cx))
            .collect()
    }

    fn render_list_item(&self, item: &ListItemType, ix: usize, cx: &Context<Self>) -> AnyElement {
        match item {
            ListItemType::Entry { entry, format } => self
                .render_history_entry(entry, *format, ix, Vec::default(), cx)
                .into_any(),
            ListItemType::SearchResult { entry, positions } => self.render_history_entry(
                entry,
                EntryTimeFormat::DateAndTime,
                ix,
                positions.clone(),
                cx,
            ),
            ListItemType::BucketSeparator(bucket) => div()
                .px(DynamicSpacing::Base06.rems(cx))
                .pt_2()
                .pb_1()
                .child(
                    Label::new(bucket.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
        }
    }

    fn render_history_entry(
        &self,
        entry: &AgentSessionInfo,
        format: EntryTimeFormat,
        ix: usize,
        highlight_positions: Vec<usize>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let selected = ix == self.selected_index;
        let hovered = Some(ix) == self.hovered_index;

        let (timestamp, display_text, full_date) = if let Some(updated_at) = entry.updated_at {
            let timestamp = updated_at.timestamp();

            let display_text = match format {
                EntryTimeFormat::DateAndTime => {
                    let now = Utc::now();
                    let duration = now.signed_duration_since(updated_at);
                    let days = duration.num_days();

                    format!("{}d", days)
                }
                EntryTimeFormat::TimeOnly => {
                    format.format_timestamp(timestamp, self.local_timezone)
                }
            };

            let full_date =
                EntryTimeFormat::DateAndTime.format_timestamp(timestamp, self.local_timezone);

            (Some(timestamp), display_text, Some(full_date))
        } else {
            (None, String::new(), None)
        };

        let title = session_title(entry);
        let can_delete = self.delete_supported;

        h_flex()
            .w_full()
            .pb_1()
            .child(
                ListItem::new(ix)
                    .rounded()
                    .toggle_state(selected)
                    .spacing(ListItemSpacing::Sparse)
                    .start_slot(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .justify_between()
                            .child(
                                HighlightedLabel::new(title.clone(), highlight_positions)
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .children(timestamp.map(|_timestamp| {
                                Label::new(display_text)
                                    .color(Color::Muted)
                                    .size(LabelSize::XSmall)
                                    .into_any_element()
                            })),
                    )
                    .when_some(full_date, |this, full_date| {
                        this.tooltip(move |_, cx| {
                            Tooltip::with_meta(title.clone(), None, full_date.clone(), cx)
                        })
                    })
                    .on_hover(cx.listener(move |this, is_hovered, _window, cx| {
                        if *is_hovered {
                            this.hovered_index = Some(ix);
                        } else if this.hovered_index == Some(ix) {
                            this.hovered_index = None;
                        }

                        cx.notify();
                    }))
                    .end_slot::<IconButton>(if hovered && can_delete {
                        Some(
                            IconButton::new("delete", IconName::Trash)
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action("Delete", &RemoveSelectedThread, cx)
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.remove_thread(ix, cx);
                                    cx.stop_propagation()
                                })),
                        )
                    } else {
                        None
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.confirm_entry(ix, cx))),
            )
            .into_any_element()
    }
}

fn session_title(session: &AgentSessionInfo) -> SharedString {
    session.display_title()
}

fn build_bucketed_items(sessions: Vec<AgentSessionInfo>) -> Vec<ListItemType> {
    let mut items = Vec::with_capacity(sessions.len() + 1);
    let mut bucket = None;
    let today = Local::now().naive_local().date();

    for session in sessions.into_iter() {
        let entry_bucket = if let Some(updated_at) = session.updated_at {
            let entry_date = updated_at.with_timezone(&Local).naive_local().date();
            TimeBucket::from_dates(today, entry_date)
        } else {
            TimeBucket::Undated
        };

        if Some(entry_bucket) != bucket {
            bucket = Some(entry_bucket);
            items.push(ListItemType::BucketSeparator(entry_bucket));
        }

        items.push(ListItemType::Entry {
            entry: session,
            format: entry_bucket.into(),
        });
    }

    items
}

async fn build_search_items(
    sessions: Vec<AgentSessionInfo>,
    query: SharedString,
    background_executor: BackgroundExecutor,
) -> Vec<ListItemType> {
    let mut candidates = Vec::with_capacity(sessions.len());
    for (idx, session) in sessions.iter().enumerate() {
        let title = session_title(session);
        candidates.push(StringMatchCandidate::new(idx, title.as_ref()));
    }

    const MAX_MATCHES: usize = 100;

    let matches = fuzzy::match_strings(
        &candidates,
        &query,
        false,
        true,
        MAX_MATCHES,
        &Default::default(),
        background_executor,
    )
    .await;

    matches
        .into_iter()
        .map(|search_match| ListItemType::SearchResult {
            entry: sessions[search_match.candidate_id].clone(),
            positions: search_match.positions,
        })
        .collect()
}

impl Focusable for AcpThreadHistory {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_editor.focus_handle(cx)
    }
}

impl Render for AcpThreadHistory {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_no_history = self.visible_items.is_empty() && self.search_query.is_empty();

        v_flex()
            .key_context("ThreadHistory")
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::remove_selected_thread))
            .on_action(cx.listener(|this, _: &RemoveHistory, window, cx| {
                this.remove_history(window, cx);
            }))
            .child(
                h_flex()
                    .h(Tab::container_height(cx))
                    .w_full()
                    .py_1()
                    .px_2()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .color(Color::Muted)
                            .size(IconSize::Small),
                    )
                    .child(self.search_editor.clone()),
            )
            .child({
                let view = v_flex()
                    .id("list-container")
                    .relative()
                    .overflow_hidden()
                    .flex_grow();

                if has_no_history {
                    view.justify_center().items_center().child(
                        Label::new("You don't have any past threads yet.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                } else if self.search_produced_no_matches() {
                    view.justify_center()
                        .items_center()
                        .child(Label::new("No threads match your search.").size(LabelSize::Small))
                } else {
                    view.child(
                        uniform_list(
                            "thread-history",
                            self.visible_items.len(),
                            cx.processor(|this, range: Range<usize>, window, cx| {
                                this.render_list_items(range, window, cx)
                            }),
                        )
                        .p_1()
                        .pr_4()
                        .track_scroll(&self.scroll_handle)
                        .flex_grow(),
                    )
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx)
                }
            })
            .when(!has_no_history, |this| {
                this.child(
                    h_flex()
                        .p_2()
                        .border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                        .when(!self.confirming_delete_history, |this| {
                            this.child(
                                Button::new("delete_history", "Delete All History")
                                    .disabled(!self.delete_all_supported)
                                    .full_width()
                                    .style(ButtonStyle::Outlined)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if !this.delete_all_supported {
                                            return;
                                        }
                                        this.prompt_delete_history(window, cx);
                                    })),
                            )
                        })
                        .when(self.confirming_delete_history, |this| {
                            this.w_full()
                                .gap_2()
                                .flex_wrap()
                                .justify_between()
                                .child(
                                    h_flex()
                                        .flex_wrap()
                                        .gap_1()
                                        .child(
                                            Label::new("Delete all threads?")
                                                .size(LabelSize::Small),
                                        )
                                        .child(
                                            Label::new("You won't be able to recover them later.")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        ),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Button::new("cancel_delete", "Cancel")
                                                .label_size(LabelSize::Small)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.cancel_delete_history(window, cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("confirm_delete", "Delete")
                                                .style(ButtonStyle::Tinted(ui::TintColor::Error))
                                                .color(Color::Error)
                                                .label_size(LabelSize::Small)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.remove_history(window, cx);
                                                })),
                                        ),
                                )
                        }),
                )
            })
    }
}

#[derive(Clone, Copy)]
pub enum EntryTimeFormat {
    DateAndTime,
    TimeOnly,
}

impl EntryTimeFormat {
    fn format_timestamp(&self, timestamp: i64, timezone: UtcOffset) -> String {
        let Ok(timestamp) = OffsetDateTime::from_unix_timestamp(timestamp) else {
            return String::new();
        };

        match self {
            EntryTimeFormat::DateAndTime => time_format::format_localized_timestamp(
                timestamp,
                OffsetDateTime::now_utc(),
                timezone,
                time_format::TimestampFormat::EnhancedAbsolute,
            ),
            EntryTimeFormat::TimeOnly => time_format::format_time(timestamp.to_offset(timezone)),
        }
    }
}

impl From<TimeBucket> for EntryTimeFormat {
    fn from(bucket: TimeBucket) -> Self {
        match bucket {
            TimeBucket::Today => EntryTimeFormat::TimeOnly,
            TimeBucket::Yesterday => EntryTimeFormat::TimeOnly,
            TimeBucket::ThisWeek => EntryTimeFormat::DateAndTime,
            TimeBucket::PastWeek => EntryTimeFormat::DateAndTime,
            TimeBucket::All => EntryTimeFormat::DateAndTime,
            TimeBucket::Undated => EntryTimeFormat::DateAndTime,
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum TimeBucket {
    Today,
    Yesterday,
    ThisWeek,
    PastWeek,
    All,
    Undated,
}

impl TimeBucket {
    fn from_dates(reference: NaiveDate, date: NaiveDate) -> Self {
        if date == reference {
            return TimeBucket::Today;
        }

        if date == reference - TimeDelta::days(1) {
            return TimeBucket::Yesterday;
        }

        let week = date.iso_week();

        if reference.iso_week() == week {
            return TimeBucket::ThisWeek;
        }

        let last_week = (reference - TimeDelta::days(7)).iso_week();

        if week == last_week {
            return TimeBucket::PastWeek;
        }

        TimeBucket::All
    }
}

impl Display for TimeBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimeBucket::Today => write!(f, "Today"),
            TimeBucket::Yesterday => write!(f, "Yesterday"),
            TimeBucket::ThisWeek => write!(f, "This Week"),
            TimeBucket::PastWeek => write!(f, "Past Week"),
            TimeBucket::All => write!(f, "All"),
            TimeBucket::Undated => write!(f, "Other"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone as _, Utc};

    #[test]
    fn test_time_bucket_from_dates() {
        let today = NaiveDate::from_ymd_opt(2023, 1, 15).unwrap();

        let date = today;
        assert_eq!(TimeBucket::from_dates(today, date), TimeBucket::Today);

        let date = NaiveDate::from_ymd_opt(2023, 1, 14).unwrap();
        assert_eq!(TimeBucket::from_dates(today, date), TimeBucket::Yesterday);

        let date = NaiveDate::from_ymd_opt(2023, 1, 13).unwrap();
        assert_eq!(TimeBucket::from_dates(today, date), TimeBucket::ThisWeek);

        let date = NaiveDate::from_ymd_opt(2023, 1, 11).unwrap();
        assert_eq!(TimeBucket::from_dates(today, date), TimeBucket::ThisWeek);

        let date = NaiveDate::from_ymd_opt(2023, 1, 8).unwrap();
        assert_eq!(TimeBucket::from_dates(today, date), TimeBucket::PastWeek);

        let date = NaiveDate::from_ymd_opt(2023, 1, 5).unwrap();
        assert_eq!(TimeBucket::from_dates(today, date), TimeBucket::PastWeek);

        // All: not in this week or last week
        let date = NaiveDate::from_ymd_opt(2023, 1, 1).unwrap();
        assert_eq!(TimeBucket::from_dates(today, date), TimeBucket::All);

        // Test year boundary cases
        let new_year = NaiveDate::from_ymd_opt(2023, 1, 1).unwrap();

        let date = NaiveDate::from_ymd_opt(2022, 12, 31).unwrap();
        assert_eq!(
            TimeBucket::from_dates(new_year, date),
            TimeBucket::Yesterday
        );

        let date = NaiveDate::from_ymd_opt(2022, 12, 28).unwrap();
        assert_eq!(TimeBucket::from_dates(new_year, date), TimeBucket::ThisWeek);
    }

    #[test]
    fn test_build_bucketed_items_includes_missing_updated_at_and_inserts_separators() {
        let sessions = vec![
            AgentSessionInfo {
                session_id: acp::SessionId::new("no-updated-at"),
                cwd: None,
                title: Some("No UpdatedAt".into()),
                updated_at: None,
                meta: None,
            },
            AgentSessionInfo {
                session_id: acp::SessionId::new("today-1"),
                cwd: None,
                title: Some("Today 1".into()),
                updated_at: Some(Utc.with_ymd_and_hms(2099, 1, 2, 12, 0, 0).unwrap()),
                meta: None,
            },
            AgentSessionInfo {
                session_id: acp::SessionId::new("today-2"),
                cwd: None,
                title: Some("Today 2".into()),
                updated_at: Some(Utc.with_ymd_and_hms(2099, 1, 2, 10, 0, 0).unwrap()),
                meta: None,
            },
        ];

        let items = build_bucketed_items(sessions);

        // The item with no updated_at should be rendered.
        assert!(
            items.iter().any(|item| {
                item.session()
                    .is_some_and(|s| s.session_id == acp::SessionId::new("no-updated-at"))
            }),
            "sessions missing updated_at should be rendered in bucketed view"
        );

        // We should have exactly 2 bucket separators for undated + today's sessions.
        let bucket_separators = items
            .iter()
            .filter(|item| matches!(item, ListItemType::BucketSeparator(_)))
            .count();
        assert_eq!(bucket_separators, 2);

        // And 3 entries.
        let entry_count = items
            .iter()
            .filter(|item| matches!(item, ListItemType::Entry { .. }))
            .count();
        assert_eq!(entry_count, 3);
    }

    #[test]
    fn test_session_title_fallback_and_trimming() {
        let session_empty = AgentSessionInfo {
            session_id: acp::SessionId::new("empty"),
            cwd: None,
            title: Some("   ".into()),
            updated_at: Some(Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap()),
            meta: None,
        };
        assert_eq!(session_title(&session_empty).as_ref(), "New Thread");

        let session_none = AgentSessionInfo {
            session_id: acp::SessionId::new("none"),
            cwd: None,
            title: None,
            updated_at: Some(Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap()),
            meta: None,
        };
        assert_eq!(session_title(&session_none).as_ref(), "New Thread");

        let session_title_ok = AgentSessionInfo {
            session_id: acp::SessionId::new("ok"),
            cwd: None,
            title: Some("Hello".into()),
            updated_at: Some(Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap()),
            meta: None,
        };
        assert_eq!(session_title(&session_title_ok).as_ref(), "Hello");
    }
}

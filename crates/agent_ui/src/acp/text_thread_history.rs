use crate::{RemoveHistory, RemoveSelectedThread};
use assistant_text_thread::{SavedTextThreadMetadata, TextThreadStore};
use chrono::{Datelike as _, Local, NaiveDate, TimeDelta};
use editor::{Editor, EditorEvent};
use fuzzy::StringMatchCandidate;
use gpui::{
    App, BackgroundExecutor, Entity, EventEmitter, FocusHandle, Focusable, ScrollStrategy, Task,
    UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use std::{fmt::Display, ops::Range, path::Path, sync::Arc};
use text::Bias;
use time::{OffsetDateTime, UtcOffset};
use ui::{
    HighlightedLabel, IconButtonShape, ListItem, ListItemSpacing, Tab, Tooltip, WithScrollbar,
    prelude::*,
};
use util::ResultExt as _;

pub struct TextThreadHistory {
    text_thread_store: WeakEntity<TextThreadStore>,
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
        entry: SavedTextThreadMetadata,
        format: EntryTimeFormat,
    },
    SearchResult {
        entry: SavedTextThreadMetadata,
        positions: Vec<usize>,
    },
}

impl ListItemType {
    fn entry(&self) -> Option<&SavedTextThreadMetadata> {
        match self {
            ListItemType::Entry { entry, .. } => Some(entry),
            ListItemType::SearchResult { entry, .. } => Some(entry),
            _ => None,
        }
    }
}

pub enum TextThreadHistoryEvent {
    OpenTextThread(Arc<Path>),
}

impl EventEmitter<TextThreadHistoryEvent> for TextThreadHistory {}

impl TextThreadHistory {
    pub(crate) fn new(
        text_thread_store: WeakEntity<TextThreadStore>,
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

        let local_timezone =
            UtcOffset::from_whole_seconds(chrono::Local::now().offset().local_minus_utc())
                .unwrap_or_else(|_| UtcOffset::UTC);

        let mut this = Self {
            text_thread_store,
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

    fn update_visible_items(&mut self, preserve_selected_item: bool, cx: &mut Context<Self>) {
        let Some(store) = self.text_thread_store.upgrade() else {
            return;
        };

        let mut threads: Vec<SavedTextThreadMetadata> =
            store.read(cx).unordered_text_threads().cloned().collect();
        threads.sort_by(|a, b| b.mtime.cmp(&a.mtime));

        let query = self.search_query.clone();
        let selected_path = if preserve_selected_item {
            self.selected_path().cloned()
        } else {
            None
        };

        let background_executor = cx.background_executor().clone();

        self._update_task = cx.spawn(async move |this, cx| {
            let new_visible_items = cx
                .background_spawn(async move {
                    if query.is_empty() {
                        build_bucketed_items(threads)
                    } else {
                        build_search_items(threads, query, background_executor).await
                    }
                })
                .await;

            if let Err(err) = this.update(cx, |this, cx| {
                let new_selected_index = if let Some(path) = selected_path {
                    new_visible_items
                        .iter()
                        .position(|visible_entry| {
                            visible_entry
                                .entry()
                                .is_some_and(|entry| entry.path == path)
                        })
                        .unwrap_or(0)
                } else {
                    0
                };

                this.visible_items = new_visible_items;
                this.set_selected_index(new_selected_index, Bias::Right, cx);
                cx.notify();
            }) {
                log::error!("Failed to update text thread history UI: {err:?}");
            }
        });
    }

    fn search_produced_no_matches(&self) -> bool {
        self.visible_items.is_empty() && !self.search_query.is_empty()
    }

    fn selected_path(&self) -> Option<&Arc<Path>> {
        self.get_entry(self.selected_index).map(|entry| &entry.path)
    }

    fn get_entry(&self, visible_items_ix: usize) -> Option<&SavedTextThreadMetadata> {
        self.visible_items.get(visible_items_ix)?.entry()
    }

    fn set_selected_index(&mut self, mut index: usize, bias: Bias, cx: &mut Context<Self>) {
        if self.visible_items.is_empty() {
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
        let Some(entry) = self.get_entry(ix) else {
            return;
        };
        cx.emit(TextThreadHistoryEvent::OpenTextThread(entry.path.clone()));
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
        let Some(entry) = self.get_entry(visible_item_ix) else {
            return;
        };

        let Some(store) = self.text_thread_store.upgrade() else {
            return;
        };

        let path = entry.path.clone();
        let delete_task = store.update(cx, |store, cx| store.delete_local(path, cx));

        cx.spawn(async move |this, cx| {
            if let Err(err) = delete_task.await {
                log::error!("Failed to delete text thread: {err:?}");
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
        let Some(store) = self.text_thread_store.upgrade() else {
            self.confirming_delete_history = false;
            cx.notify();
            return;
        };

        let paths: Vec<Arc<Path>> = store
            .read(cx)
            .unordered_text_threads()
            .map(|t| t.path.clone())
            .collect();

        self.confirming_delete_history = false;

        cx.spawn(async move |this, cx| {
            for path in paths {
                store.update(cx, |store, cx| store.delete_local(path, cx));
            }

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
        entry: &SavedTextThreadMetadata,
        format: EntryTimeFormat,
        ix: usize,
        highlight_positions: Vec<usize>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let selected = ix == self.selected_index;
        let hovered = Some(ix) == self.hovered_index;

        let timestamp = entry.mtime.timestamp();

        let display_text = match format {
            EntryTimeFormat::DateAndTime => {
                let now = Local::now();
                let duration = now.signed_duration_since(entry.mtime);
                let days = duration.num_days();
                format!("{}d", days)
            }
            EntryTimeFormat::TimeOnly => format.format_timestamp(timestamp, self.local_timezone),
        };

        let full_date =
            EntryTimeFormat::DateAndTime.format_timestamp(timestamp, self.local_timezone);

        let title = entry_title(entry);

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
                            .child(
                                Label::new(display_text)
                                    .color(Color::Muted)
                                    .size(LabelSize::XSmall)
                                    .into_any_element(),
                            ),
                    )
                    .tooltip(move |_, cx| {
                        Tooltip::with_meta(title.clone(), None, full_date.clone(), cx)
                    })
                    .on_hover(cx.listener(move |this, is_hovered, _window, cx| {
                        if *is_hovered {
                            this.hovered_index = Some(ix);
                        } else if this.hovered_index == Some(ix) {
                            this.hovered_index = None;
                        }

                        cx.notify();
                    }))
                    .end_slot::<IconButton>(if hovered {
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

fn entry_title(entry: &SavedTextThreadMetadata) -> SharedString {
    let title = entry.title.trim();
    if title.is_empty() {
        SharedString::from("New Thread")
    } else {
        entry.title.clone()
    }
}

fn build_bucketed_items(threads: Vec<SavedTextThreadMetadata>) -> Vec<ListItemType> {
    let mut items = Vec::with_capacity(threads.len() + 1);
    let mut bucket = None;
    let today = Local::now().naive_local().date();

    for thread in threads.into_iter() {
        let entry_date = thread.mtime.naive_local().date();
        let entry_bucket = TimeBucket::from_dates(today, entry_date);

        if Some(entry_bucket) != bucket {
            bucket = Some(entry_bucket);
            items.push(ListItemType::BucketSeparator(entry_bucket));
        }

        items.push(ListItemType::Entry {
            entry: thread,
            format: entry_bucket.into(),
        });
    }

    items
}

async fn build_search_items(
    threads: Vec<SavedTextThreadMetadata>,
    query: SharedString,
    background_executor: BackgroundExecutor,
) -> Vec<ListItemType> {
    let mut candidates = Vec::with_capacity(threads.len());
    for (idx, thread) in threads.iter().enumerate() {
        let title = entry_title(thread);
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
            entry: threads[search_match.candidate_id].clone(),
            positions: search_match.positions,
        })
        .collect()
}

impl Focusable for TextThreadHistory {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_editor.focus_handle(cx)
    }
}

impl Render for TextThreadHistory {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_no_history = self.visible_items.is_empty() && self.search_query.is_empty();

        v_flex()
            .key_context("TextThreadHistory")
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
                            "text-thread-history",
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
                                    .full_width()
                                    .style(ButtonStyle::Outlined)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
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
enum EntryTimeFormat {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, NaiveDate, TimeDelta};

    #[test]
    fn test_time_bucket_from_dates() {
        let today = NaiveDate::from_ymd_opt(2024, 6, 15).unwrap(); // Saturday

        assert_eq!(TimeBucket::from_dates(today, today), TimeBucket::Today);

        let yesterday = today - TimeDelta::days(1);
        assert_eq!(
            TimeBucket::from_dates(today, yesterday),
            TimeBucket::Yesterday
        );

        let earlier_this_week = today - TimeDelta::days(5);
        assert_eq!(
            TimeBucket::from_dates(today, earlier_this_week),
            TimeBucket::ThisWeek
        );

        let last_week = today - TimeDelta::days(10);
        assert_eq!(
            TimeBucket::from_dates(today, last_week),
            TimeBucket::PastWeek
        );

        let long_ago = today - TimeDelta::days(30);
        assert_eq!(TimeBucket::from_dates(today, long_ago), TimeBucket::All);
    }

    #[test]
    fn test_entry_title_fallback_and_trimming() {
        fn make_entry(title: &str) -> SavedTextThreadMetadata {
            SavedTextThreadMetadata {
                title: title.to_string().into(),
                path: Arc::from(Path::new("/test/path.json")),
                mtime: Local::now(),
            }
        }

        assert_eq!(entry_title(&make_entry("My Thread")).as_ref(), "My Thread");
        assert_eq!(entry_title(&make_entry("")).as_ref(), "New Thread");
        assert_eq!(entry_title(&make_entry("   ")).as_ref(), "New Thread");
        assert_eq!(
            entry_title(&make_entry("  Trimmed Title  ")).as_ref(),
            "  Trimmed Title  "
        );
    }

    #[test]
    fn test_build_bucketed_items_inserts_separators() {
        let today = Local::now();
        let yesterday = today - TimeDelta::days(1);

        let threads = vec![
            SavedTextThreadMetadata {
                title: "Thread 1".into(),
                path: Arc::from(Path::new("/test/1.json")),
                mtime: today,
            },
            SavedTextThreadMetadata {
                title: "Thread 2".into(),
                path: Arc::from(Path::new("/test/2.json")),
                mtime: today,
            },
            SavedTextThreadMetadata {
                title: "Thread 3".into(),
                path: Arc::from(Path::new("/test/3.json")),
                mtime: yesterday,
            },
        ];

        let items = build_bucketed_items(threads);

        // Should have: Today separator, Thread 1, Thread 2, Yesterday separator, Thread 3
        assert_eq!(items.len(), 5);

        assert!(matches!(
            &items[0],
            ListItemType::BucketSeparator(TimeBucket::Today)
        ));
        assert!(
            matches!(&items[1], ListItemType::Entry { entry, .. } if entry.title == "Thread 1")
        );
        assert!(
            matches!(&items[2], ListItemType::Entry { entry, .. } if entry.title == "Thread 2")
        );
        assert!(matches!(
            &items[3],
            ListItemType::BucketSeparator(TimeBucket::Yesterday)
        ));
        assert!(
            matches!(&items[4], ListItemType::Entry { entry, .. } if entry.title == "Thread 3")
        );
    }

    #[test]
    fn test_build_bucketed_items_empty_input() {
        let items = build_bucketed_items(vec![]);
        assert!(items.is_empty());
    }

    #[gpui::test]
    async fn test_build_search_items_fuzzy_matching(cx: &mut gpui::TestAppContext) {
        let threads = vec![
            SavedTextThreadMetadata {
                title: "Implement user authentication".to_string().into(),
                path: Arc::from(Path::new("/test/1.json")),
                mtime: Local::now(),
            },
            SavedTextThreadMetadata {
                title: "Fix database connection".to_string().into(),
                path: Arc::from(Path::new("/test/2.json")),
                mtime: Local::now(),
            },
            SavedTextThreadMetadata {
                title: "Add user profile page".to_string().into(),
                path: Arc::from(Path::new("/test/3.json")),
                mtime: Local::now(),
            },
            SavedTextThreadMetadata {
                title: "Refactor authentication module".to_string().into(),
                path: Arc::from(Path::new("/test/4.json")),
                mtime: Local::now(),
            },
        ];

        let executor = cx.executor();

        // Search for "auth" - should match threads with "authentication"
        let items = build_search_items(threads.clone(), "auth".into(), executor.clone()).await;
        assert_eq!(items.len(), 2);

        // Verify both auth-related threads are found
        let titles: Vec<_> = items
            .iter()
            .filter_map(|item| item.entry().map(|e| e.title.as_ref()))
            .collect();
        assert!(titles.contains(&"Implement user authentication"));
        assert!(titles.contains(&"Refactor authentication module"));

        // Verify search results have highlight positions
        for item in &items {
            if let ListItemType::SearchResult { positions, .. } = item {
                assert!(
                    !positions.is_empty(),
                    "Search results should have highlight positions"
                );
            } else {
                panic!("Expected SearchResult variant");
            }
        }

        // Search for "user" - should match threads with "user"
        let items = build_search_items(threads.clone(), "user".into(), executor.clone()).await;
        assert_eq!(items.len(), 2);

        // Search with no matches
        let items = build_search_items(threads, "xyz123".into(), executor).await;
        assert!(items.is_empty());

        // Note: Empty query case is handled by the caller (update_visible_items)
        // which routes to build_bucketed_items instead of build_search_items
    }
}

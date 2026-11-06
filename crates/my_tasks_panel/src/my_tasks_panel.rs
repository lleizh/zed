use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, ParentElement, Pixels, Render,
    StatefulInteractiveElement, Styled, Task, WeakEntity, Window, actions, div, px,
};
use settings::Settings;
use workforme_common::{self, InboxItem, ItemKind};
use workforme_panel::WorkformePanelSettings;
use ui::{
    ActiveTheme, ButtonCommon, Clickable, Disableable, FluentBuilder, IconButton, IconName,
    IconSize, Label, LabelCommon, Tooltip, h_flex, v_flex,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const MY_TASKS_PANEL_KEY: &str = "MyTasksPanel";

actions!(my_tasks_panel, [ToggleFocus]);

#[derive(Debug, Clone, Copy, PartialEq)]
enum PanelTab {
    Mine,
    Team,
}

pub struct MyTasksPanel {
    focus_handle: FocusHandle,
    active_tab: PanelTab,
    items: Vec<InboxItem>,
    running: std::collections::HashSet<String>,
    existing_urls: std::collections::HashSet<String>,
    collapsed_repos: std::collections::HashSet<String>,
    author_filter: Option<String>,
    syncing: bool,
    updated_at: Option<String>,
    _workspace: WeakEntity<Workspace>,
    _poll_task: Task<()>,
}

impl EventEmitter<PanelEvent> for MyTasksPanel {}

impl MyTasksPanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let poll_task = cx.spawn({
            async move |this: WeakEntity<Self>, cx| {
                let mut ticks_since_data_sync: u64 = 60; // 初回は即 sync
                let mut ticks_since_summary_sync: u64 = 3600; // 初回は即 sync
                let mut ticks_since_auto_review: u64 = 3600; // 初回は即実行
                loop {
                    let workforme_root = {
                        let Ok(root) = this.update(cx, |_panel, cx| {
                            let s = WorkformePanelSettings::get_global(cx);
                            PathBuf::from(shellexpand::tilde(&s.workforme_root).to_string())
                        }) else {
                            break;
                        };
                        root
                    };
                    let poll_secs: u64 = 3;

                    // 60 秒ごとに sync-inbox.sh（データ取得、スクリプトのみ）
                    ticks_since_data_sync += poll_secs;
                    if ticks_since_data_sync >= 60 {
                        ticks_since_data_sync = 0;
                        let root = workforme_root.clone();
                        cx.background_executor()
                            .spawn(async move {
                                if is_working_hours() {
                                    std::process::Command::new(root.join("scripts/sync-inbox.sh"))
                                        .current_dir(&root)
                                        .output()
                                        .ok();
                                    // inbox 更新直後に re_requested を検出して redo
                                    std::process::Command::new(root.join("scripts/auto-rereview.sh"))
                                        .current_dir(&root)
                                        .spawn()
                                        .ok();
                                }
                            })
                            .await;
                    }

                    // 1 時間ごとに sync-inbox-summary.sh（agent で summary 生成）
                    ticks_since_summary_sync += poll_secs;
                    if ticks_since_summary_sync >= 3600 {
                        ticks_since_summary_sync = 0;
                        let root = workforme_root.clone();
                        cx.background_executor()
                            .spawn(async move {
                                if is_working_hours() {
                                    std::process::Command::new(root.join("scripts/sync-inbox-summary.sh"))
                                        .current_dir(&root)
                                        .output()
                                        .ok();
                                }
                            })
                            .await;
                    }

                    // 1 時間ごとに auto-review.sh（review-requested PR を順次自動レビュー）
                    ticks_since_auto_review += poll_secs;
                    if ticks_since_auto_review >= 3600 {
                        ticks_since_auto_review = 0;
                        let root = workforme_root.clone();
                        cx.background_executor()
                            .spawn(async move {
                                if is_working_hours() {
                                    std::process::Command::new(root.join("scripts/auto-review.sh"))
                                        .current_dir(&root)
                                        .spawn()
                                        .ok();
                                }
                            })
                            .await;
                    }

                    let root = workforme_root.clone();
                    let (items, updated_at, existing) = cx
                        .background_executor()
                        .spawn(async move {
                            let (items, updated_at) = load_inbox(&root);
                            let existing = load_existing_task_urls(&root);
                            (items, updated_at, existing)
                        })
                        .await;
                    let result = this.update(cx, |panel, cx| {
                        let changed = panel.items != items
                            || panel.existing_urls != existing
                            || panel.updated_at != updated_at;
                        if changed {
                            panel.items = items;
                            panel.existing_urls = existing;
                            panel.updated_at = updated_at;
                            panel.running.retain(|key| {
                                !panel.items.iter().any(|i| {
                                    format!("{}/{}", i.repo, i.number) == *key
                                        && panel.existing_urls.contains(&i.url)
                                })
                            });
                            cx.notify();
                        }
                    });
                    if result.is_err() {
                        break;
                    }
                    cx.background_executor()
                        .timer(Duration::from_secs(poll_secs))
                        .await;
                }
            }
        });

        let settings = WorkformePanelSettings::get_global(cx);
        let init_root = PathBuf::from(shellexpand::tilde(&settings.workforme_root).to_string());
        let (init_items, init_updated_at) = load_inbox(&init_root);
        let init_existing = load_existing_task_urls(&init_root);

        Self {
            focus_handle: cx.focus_handle(),
            active_tab: PanelTab::Mine,
            items: init_items,
            running: std::collections::HashSet::new(),
            existing_urls: init_existing,
            collapsed_repos: std::collections::HashSet::new(),
            author_filter: None,
            syncing: false,
            updated_at: init_updated_at,
            _workspace: workspace,
            _poll_task: poll_task,
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let s = WorkformePanelSettings::get_global(cx);
        let root = PathBuf::from(shellexpand::tilde(&s.workforme_root).to_string());

        self.syncing = true;
        cx.notify();

        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            // sync-inbox.sh を実行
            cx.background_executor()
                .spawn({
                    let root = root.clone();
                    async move {
                        std::process::Command::new(root.join("scripts/sync-inbox.sh"))
                            .current_dir(&root)
                            .output()
                            .ok();
                    }
                })
                .await;

            // スクリプトは bg agent を起動するだけなので即読み込み
            let (items, updated_at, existing) = cx
                .background_executor()
                .spawn(async move {
                    let (items, updated_at) = load_inbox(&root);
                    let existing = load_existing_task_urls(&root);
                    (items, updated_at, existing)
                })
                .await;
            this.update(cx, |panel, cx| {
                panel.items = items;
                panel.existing_urls = existing;
                panel.updated_at = updated_at;
                panel.syncing = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn filtered_items(&self, tab: PanelTab) -> Vec<&InboxItem> {
        self.items
            .iter()
            .filter(|item| match tab {
                PanelTab::Mine => item.role == "assignee" || item.role == "reviewer" || item.role == "author" || item.role == "created",
                PanelTab::Team => item.role == "team",
            })
            .filter(|item| {
                if let Some(ref filter) = self.author_filter {
                    item.author.as_deref() == Some(filter.as_str())
                } else {
                    true
                }
            })
            .collect()
    }

    fn available_authors(&self, tab: PanelTab) -> Vec<String> {
        let mut authors: Vec<String> = self.items
            .iter()
            .filter(|item| match tab {
                PanelTab::Mine => item.role == "assignee" || item.role == "reviewer" || item.role == "author" || item.role == "created",
                PanelTab::Team => item.role == "team",
            })
            .filter_map(|item| item.author.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        authors.sort();
        authors
    }

    fn render_item_list(
        &self,
        items: Vec<&InboxItem>,
        id_prefix: &str,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut repos: Vec<String> = items.iter().map(|i| i.repo.clone()).collect();
        repos.sort();
        repos.dedup();

        let mut sections: Vec<gpui::AnyElement> = Vec::new();
        let entity = cx.entity().downgrade();

        for repo in &repos {
            let is_collapsed = self.collapsed_repos.contains(repo);
            let repo_clone = repo.clone();
            let entity_for_repo = entity.clone();
            let item_count = items.iter().filter(|i| &i.repo == repo).count();
            sections.push(
                h_flex()
                    .id(gpui::ElementId::Name(format!("mytask-repo-{}", repo).into()))
                    .px_2()
                    .py_1()
                    .mt_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                    .child(
                        Label::new(if is_collapsed { "▶" } else { "▼" })
                            .size(ui::LabelSize::XSmall)
                            .color(ui::Color::Muted),
                    )
                    .child(
                        Label::new(format!(" {} ({})", repo, item_count))
                            .size(ui::LabelSize::Small)
                            .color(ui::Color::Default),
                    )
                    .on_click(move |_event, _window, cx| {
                        if let Some(panel) = entity_for_repo.upgrade() {
                            panel.update(cx, |panel, cx| {
                                if panel.collapsed_repos.contains(&repo_clone) {
                                    panel.collapsed_repos.remove(&repo_clone);
                                } else {
                                    panel.collapsed_repos.insert(repo_clone.clone());
                                }
                                cx.notify();
                            });
                        }
                    })
                    .into_any_element(),
            );

            if is_collapsed {
                continue;
            }

            for (idx, item) in items.iter().filter(|i| &i.repo == repo).enumerate() {
                let item_key = format!("{}/{}", item.repo, item.number);
                let is_running = self.running.contains(&item_key);
                let already_exists = self.existing_urls.contains(&item.url);
                let kind_label = match item.kind {
                    ItemKind::Issue => "issue",
                    ItemKind::PullRequest => "PR",
                };
                let kind_color = match item.kind {
                    ItemKind::Issue => gpui::rgb(0x238636),
                    ItemKind::PullRequest => gpui::rgb(0x8957e5),
                };

                let url_for_click = item.url.clone();
                let item_for_task = (*item).clone();
                let entity_clone = entity.clone();
                let item_key_clone = item_key.clone();


                sections.push(
                    v_flex()
                        .id(gpui::ElementId::NamedInteger(
                            format!("{}-{}", id_prefix, repo).into(),
                            idx as u64,
                        ))
                        .px_3()
                        .py_1()
                        .gap_0p5()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                        .on_click(move |_event, _window, cx| {
                            cx.open_url(&url_for_click);
                        })
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_0p5()
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .items_center()
                                                .child(
                                                    div()
                                                        .px_1()
                                                        .rounded_sm()
                                                        .bg(kind_color)
                                                        .child(
                                                            Label::new(kind_label)
                                                                .size(ui::LabelSize::XSmall)
                                                                .color(ui::Color::Default),
                                                        ),
                                                )
                                                .child(
                                                    Label::new(format!("#{}", item.number))
                                                        .size(ui::LabelSize::XSmall)
                                                        .color(ui::Color::Muted),
                                                )
                                                .when_some(item.author.as_ref(), |el, author| {
                                                    el.child(
                                                        Label::new(author.clone())
                                                            .size(ui::LabelSize::XSmall)
                                                            .color(ui::Color::Accent),
                                                    )
                                                })
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .overflow_hidden()
                                                        .child(
                                                            Label::new(item.title.clone())
                                                                .size(ui::LabelSize::Small)
                                                                .single_line(),
                                                        ),
                                                )
                                                .when_some(item.created_at.as_ref(), |el, created| {
                                                    let short = created.chars().take(10).collect::<String>();
                                                    el.child(
                                                        Label::new(short)
                                                            .size(ui::LabelSize::XSmall)
                                                            .color(ui::Color::Muted),
                                                    )
                                                }),
                                        )
                                        .when(!item.human_reviews.is_empty(), |el| {
                                            el.child(
                                                h_flex()
                                                    .gap_1()
                                                    .children(item.human_reviews.iter().map(|r| {
                                                        let icon = match r.state.as_str() {
                                                            "approved" => "✓",
                                                            "changes_requested" => "✗",
                                                            "re_requested" => "⟳",
                                                            "pending" => "○",
                                                            _ => "●",
                                                        };
                                                        let color = match r.state.as_str() {
                                                            "approved" => ui::Color::Success,
                                                            "changes_requested" => ui::Color::Error,
                                                            "re_requested" => ui::Color::Warning,
                                                            "pending" => ui::Color::Muted,
                                                            _ => ui::Color::Muted,
                                                        };
                                                        Label::new(format!("{}{}", icon, r.reviewer))
                                                            .size(ui::LabelSize::XSmall)
                                                            .color(color)
                                                            .into_any_element()
                                                    })),
                                            )
                                        })
                                        .when(!item.summary.is_empty(), |el| {
                                            el.child(
                                                Label::new(item.summary.clone())
                                                    .size(ui::LabelSize::XSmall)
                                                    .color(ui::Color::Muted)
                                                    .single_line(),
                                            )
                                        }),
                                )
                                .child({
                                    let run_button: Option<gpui::AnyElement> = if already_exists {
                                        Some(
                                            Label::new("✓")
                                                .size(ui::LabelSize::XSmall)
                                                .color(ui::Color::Success)
                                                .into_any_element(),
                                        )
                                    } else if is_running {
                                        Some(
                                            div()
                                                .px_1()
                                                .rounded_sm()
                                                .bg(cx.theme().colors().ghost_element_selected)
                                                .child(
                                                    Label::new("...")
                                                        .size(ui::LabelSize::XSmall)
                                                        .color(ui::Color::Muted),
                                                )
                                                .into_any_element(),
                                        )
                                    } else {
                                        let item_clone = item_for_task.clone();
                                        let entity_clone = entity_clone.clone();
                                        let key = item_key_clone.clone();
                                        Some(
                                            div()
                                                .id(gpui::ElementId::NamedInteger(
                                                    format!("{}-run-{}", id_prefix, repo).into(),
                                                    idx as u64,
                                                ))
                                                .px_1()
                                                .rounded_sm()
                                                .cursor_pointer()
                                                .bg(cx.theme().colors().element_background)
                                                .hover(|s| {
                                                    s.bg(cx.theme().colors().ghost_element_selected)
                                                })
                                                .child(
                                                    Label::new("▶")
                                                        .size(ui::LabelSize::XSmall)
                                                        .color(ui::Color::Accent),
                                                )
                                                .on_click(move |_event, _window, cx| {
                                                    cx.stop_propagation();
                                                    let item = item_clone.clone();
                                                    let entity = entity_clone.clone();
                                                    let key = key.clone();
                                                    if let Some(panel) = entity.upgrade() {
                                                        panel.update(cx, |panel, cx| {
                                                            panel.running.insert(key);
                                                            cx.notify();
                                                            let s = WorkformePanelSettings::get_global(cx);
                                                            let root = PathBuf::from(
                                                                shellexpand::tilde(&s.workforme_root).to_string(),
                                                            );
                                                            create_workforme_task(&item, &root);
                                                        });
                                                    }
                                                })
                                                .into_any_element(),
                                        )
                                    };
                                    workforme_common::render_item_status(item, run_button)
                                }),
                        )
                        .into_any_element(),
                );
            }
        }
        sections
    }
}

impl Focusable for MyTasksPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for MyTasksPanel {
    fn persistent_name() -> &'static str {
        "MyTasksPanel"
    }

    fn panel_key() -> &'static str {
        MY_TASKS_PANEL_KEY
    }

    fn position(&self, _: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Right | DockPosition::Left)
    }

    fn set_position(&mut self, _position: DockPosition, _: &mut Window, _cx: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _cx: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::AtSign)
    }

    fn icon_tooltip(&self, _: &Window, _cx: &App) -> Option<&'static str> {
        Some("My Tasks")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        12
    }
}

impl Render for MyTasksPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_tab = self.active_tab;
        let authors = self.available_authors(active_tab);
        let filtered = self.filtered_items(active_tab);
        let sections = self.render_item_list(filtered, match active_tab {
            PanelTab::Mine => "mytask",
            PanelTab::Team => "team",
        }, cx);

        let entity = cx.entity().downgrade();
        let entity_for_team = entity.clone();

        let empty_label = match active_tab {
            PanelTab::Mine => "No assigned items",
            PanelTab::Team => "No review requests",
        };

        let show_author_filter = authors.len() > 1;
        let current_filter = self.author_filter.clone();

        div()
            .id("my-tasks-panel")
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .id("tab-mine")
                                    .cursor_pointer()
                                    .px_1()
                                    .when(active_tab == PanelTab::Mine, |el| {
                                        el.border_b_2()
                                            .border_color(cx.theme().colors().text)
                                    })
                                    .child(
                                        Label::new("Mine")
                                            .size(ui::LabelSize::Small)
                                            .color(if active_tab == PanelTab::Mine {
                                                ui::Color::Default
                                            } else {
                                                ui::Color::Muted
                                            }),
                                    )
                                    .on_click({
                                        let entity = entity.clone();
                                        move |_event, _window, cx| {
                                            if let Some(panel) = entity.upgrade() {
                                                panel.update(cx, |panel, cx| {
                                                    panel.active_tab = PanelTab::Mine;
                                                    cx.notify();
                                                });
                                            }
                                        }
                                    }),
                            )
                            .child(
                                div()
                                    .id("tab-team")
                                    .cursor_pointer()
                                    .px_1()
                                    .when(active_tab == PanelTab::Team, |el| {
                                        el.border_b_2()
                                            .border_color(cx.theme().colors().text)
                                    })
                                    .child(
                                        Label::new("Team")
                                            .size(ui::LabelSize::Small)
                                            .color(if active_tab == PanelTab::Team {
                                                ui::Color::Default
                                            } else {
                                                ui::Color::Muted
                                            }),
                                    )
                                    .on_click(move |_event, _window, cx| {
                                        if let Some(panel) = entity_for_team.upgrade() {
                                            panel.update(cx, |panel, cx| {
                                                panel.active_tab = PanelTab::Team;
                                                cx.notify();
                                            });
                                        }
                                    }),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Label::new(if self.syncing {
                                    "Syncing...".to_string()
                                } else {
                                    self.updated_at
                                        .as_ref()
                                        .map(|s| format_updated_at(s))
                                        .unwrap_or_default()
                                })
                                    .size(ui::LabelSize::XSmall)
                                    .color(ui::Color::Muted),
                            )
                            .child(
                                IconButton::new("mytasks-auto-review", IconName::Sparkle)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Run auto-review"))
                                    .on_click({
                                        move |_event, _window, cx| {
                                            let s = WorkformePanelSettings::get_global(cx);
                                            let root = PathBuf::from(
                                                shellexpand::tilde(&s.workforme_root).to_string(),
                                            );
                                            std::process::Command::new(root.join("scripts/auto-review.sh"))
                                                .spawn()
                                                .ok();
                                        }
                                    }),
                            )
                            .child(
                                IconButton::new("mytasks-refresh", IconName::ArrowCircle)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Sync inbox"))
                                    .disabled(self.syncing)
                                    .on_click({
                                        let entity = cx.entity().downgrade();
                                        move |_event, _window, cx| {
                                            if let Some(panel) = entity.upgrade() {
                                                panel.update(cx, |panel, cx| {
                                                    panel.reload(cx);
                                                });
                                            }
                                        }
                                    }),
                            ),
                    ),
            )
            .when(show_author_filter, |el| {
                let entity_for_filter = cx.entity().downgrade();
                el.child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .flex_wrap()
                        .child({
                            let is_all = current_filter.is_none();
                            let entity_c = entity_for_filter.clone();
                            div()
                                .id("author-filter-all")
                                .cursor_pointer()
                                .px_1()
                                .rounded_md()
                                .when(is_all, |el| el.bg(cx.theme().colors().ghost_element_selected))
                                .when(!is_all, |el| el.hover(|s| s.bg(cx.theme().colors().ghost_element_hover)))
                                .child(
                                    Label::new("All")
                                        .size(ui::LabelSize::XSmall)
                                        .color(if is_all { ui::Color::Default } else { ui::Color::Muted }),
                                )
                                .on_click(move |_event, _window, cx| {
                                    if let Some(panel) = entity_c.upgrade() {
                                        panel.update(cx, |panel, cx| {
                                            panel.author_filter = None;
                                            cx.notify();
                                        });
                                    }
                                })
                        })
                        .children(authors.iter().enumerate().map(|(idx, author)| {
                            let is_selected = current_filter.as_deref() == Some(author.as_str());
                            let author_clone = author.clone();
                            let entity_c = entity_for_filter.clone();
                            div()
                                .id(gpui::ElementId::NamedInteger("author-filter".into(), idx as u64))
                                .cursor_pointer()
                                .px_1()
                                .rounded_md()
                                .when(is_selected, |el| el.bg(cx.theme().colors().ghost_element_selected))
                                .when(!is_selected, |el| el.hover(|s| s.bg(cx.theme().colors().ghost_element_hover)))
                                .child(
                                    Label::new(author.clone())
                                        .size(ui::LabelSize::XSmall)
                                        .color(if is_selected { ui::Color::Default } else { ui::Color::Muted }),
                                )
                                .on_click(move |_event, _window, cx| {
                                    if let Some(panel) = entity_c.upgrade() {
                                        let author = author_clone.clone();
                                        panel.update(cx, |panel, cx| {
                                            if panel.author_filter.as_deref() == Some(author.as_str()) {
                                                panel.author_filter = None;
                                            } else {
                                                panel.author_filter = Some(author);
                                            }
                                            cx.notify();
                                        });
                                    }
                                })
                                .into_any_element()
                        })),
                )
            })
            .child(
                div()
                    .id("mytasks-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .when(sections.is_empty(), |el| {
                        el.child(
                            v_flex()
                                .p_4()
                                .gap_2()
                                .items_center()
                                .child(
                                    Label::new(empty_label)
                                        .size(ui::LabelSize::Small)
                                        .color(ui::Color::Muted),
                                ),
                        )
                    })
                    .children(sections),
            )
    }
}

fn is_working_hours() -> bool {
    // 平日 9:00-20:00 (JST) のみ true
    let output = std::process::Command::new("date")
        .args(["+%u %H"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let parts: Vec<&str> = output.split_whitespace().collect();
    if parts.len() != 2 {
        return true;
    }
    let dow: u32 = parts[0].parse().unwrap_or(1);
    let hour: u32 = parts[1].parse().unwrap_or(12);
    dow <= 5 && hour >= 10 && hour < 19
}

fn format_updated_at(iso: &str) -> String {
    // ISO8601 UTC → JST (UTC+9) の HH:MM 表示
    let time_part = match iso.split('T').nth(1) {
        Some(t) => t,
        None => return iso.to_string(),
    };
    let hm: String = time_part.chars().take(5).collect();
    let parts: Vec<&str> = hm.split(':').collect();
    if parts.len() == 2 {
        if let (Ok(h), Ok(m)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
            let jst_h = (h + 9) % 24;
            return format!("{:02}:{:02}", jst_h, m);
        }
    }
    hm
}

fn load_inbox(workforme_root: &PathBuf) -> (Vec<InboxItem>, Option<String>) {
    let inbox = workforme_common::load_inbox(workforme_root);
    (inbox.items, inbox.updated_at)
}

fn load_existing_task_urls(workforme_root: &PathBuf) -> std::collections::HashSet<String> {
    let mut urls = std::collections::HashSet::new();
    let work_dir = workforme_root.join(".work");
    let Ok(entries) = std::fs::read_dir(&work_dir) else {
        return urls;
    };
    for entry in entries.flatten() {
        let state_path = entry.path().join("state.json");
        let Ok(content) = std::fs::read_to_string(&state_path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if let Some(url) = val.get("url").and_then(|v| v.as_str()) {
            urls.insert(url.to_string());
        }
        if let Some(pr_url) = val.get("pr_url").and_then(|v| v.as_str()) {
            urls.insert(pr_url.to_string());
        }
    }
    urls
}

fn create_workforme_task(item: &InboxItem, workforme_root: &PathBuf) {
    let (workflow_type, first_stage) = match item.kind {
        ItemKind::Issue => ("issue-dev", "plan"),
        ItemKind::PullRequest => ("pr-review", "review-pr"),
    };

    let root = workforme_root.clone();
    let url = item.url.clone();
    std::thread::spawn(move || {
        let output = std::process::Command::new(root.join("scripts/create-task.sh"))
            .arg(workflow_type)
            .arg(&url)
            .current_dir(&root)
            .output();
        if let Ok(out) = output {
            if out.status.success() {
                let task_dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !task_dir.is_empty() {
                    std::process::Command::new(root.join("scripts/run-stage.sh"))
                        .arg(&task_dir)
                        .arg(first_stage)
                        .current_dir(&root)
                        .spawn()
                        .ok();
                }
            }
        }
    });
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<MyTasksPanel>(window, cx);
        });
    })
    .detach();
}

pub async fn load(
    workspace: WeakEntity<Workspace>,
    mut cx: AsyncWindowContext,
) -> anyhow::Result<Entity<MyTasksPanel>> {
    let weak = workspace.clone();
    workspace
        .update_in(&mut cx, |_workspace, _window, cx| {
            cx.new(|cx| MyTasksPanel::new(weak, cx))
        })
        .context("failed to create MyTasksPanel")
}
